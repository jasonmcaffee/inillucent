#!/usr/bin/env node
// Copies `agent-skills/` into the two directories agents actually read.
//
//   node tools/sync-skills.mjs          # write the copies
//   node tools/sync-skills.mjs --check  # report a difference and exit non-zero
//
// **Copies, not symlinks (task-1961, S1).** `agent-skills/README.md` used to
// tell a person to run `ln -s` into their own user-level skills directory by
// hand, and nothing in
// the repository ran it - so every fresh clone started with the eight skills
// invisible to Claude Code's own matcher, which reads a project's
// `.claude/skills/`. A symlink is not the fix either: this repository is
// developed on Windows, and a checkout without `core.symlinks` turns one into a
// text file containing a path, which no agent follows.
//
// So the copies are committed, and
// `crates/inillucent-compat/tests/documentation.rs` fails when any copy differs
// from its source by a byte. `agent-skills/` stays the source of truth: it is
// the tool-neutral directory anything else can be pointed at.

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const SOURCE = path.join(ROOT, 'agent-skills');

// Where each agent looks for a project's skills.
//
// `.claude/skills/` is Claude Code's. `.agents/skills/` is the Agent Skills
// layout Codex and the other adopters read; it is checked against the current
// Codex documentation in `agent-skills/README.md`, which records the answer and
// the date it was checked, because a path an agent reads is a fact about
// somebody else's product and can move.
const TARGETS = ['.claude/skills', '.agents/skills'];

/** Returns every skill directory name under `agent-skills/`. */
function skills() {
  return fs
    .readdirSync(SOURCE, { withFileTypes: true })
    .filter((entry) => entry.isDirectory())
    .map((entry) => entry.name)
    .filter((name) => fs.existsSync(path.join(SOURCE, name, 'SKILL.md')))
    .sort();
}

/**
 * Returns every file inside one skill, relative to the skill's own directory.
 *
 * A skill is usually one `SKILL.md`, and the copy has to carry whatever else is
 * beside it rather than only that file.
 *
 * @param {string} name - the skill's directory name
 */
function filesOf(name) {
  const base = path.join(SOURCE, name);
  const found = [];
  const walk = (directory, prefix) => {
    for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
      const relative = prefix ? `${prefix}/${entry.name}` : entry.name;
      if (entry.isDirectory()) walk(path.join(directory, entry.name), relative);
      else found.push(relative);
    }
  };
  walk(base, '');
  return found.sort();
}

/** Writes the copies, or reports what differs. */
function main() {
  const check = process.argv.includes('--check');
  const problems = [];
  let written = 0;
  for (const target of TARGETS) {
    for (const name of skills()) {
      for (const file of filesOf(name)) {
        const from = path.join(SOURCE, name, file);
        const to = path.join(ROOT, target, name, file);
        const body = fs.readFileSync(from);
        if (check) {
          if (!fs.existsSync(to)) {
            problems.push(`${target}/${name}/${file} is missing`);
          } else if (!fs.readFileSync(to).equals(body)) {
            problems.push(`${target}/${name}/${file} differs from agent-skills/${name}/${file}`);
          }
          continue;
        }
        fs.mkdirSync(path.dirname(to), { recursive: true });
        fs.writeFileSync(to, body);
        written += 1;
      }
    }
  }

  // A copy of a skill that no longer exists is worse than a missing one: an
  // agent reads it and acts on a page nobody maintains.
  for (const target of TARGETS) {
    const base = path.join(ROOT, target);
    if (!fs.existsSync(base)) continue;
    const known = new Set(skills());
    for (const entry of fs.readdirSync(base, { withFileTypes: true })) {
      if (!entry.isDirectory() || known.has(entry.name)) continue;
      if (check) {
        problems.push(`${target}/${entry.name} is a copy of a skill that no longer exists`);
      } else {
        fs.rmSync(path.join(base, entry.name), { recursive: true, force: true });
        console.log(`removed ${target}/${entry.name}`);
      }
    }
  }

  if (check) {
    if (problems.length === 0) {
      console.log(`every skill copy matches agent-skills/ (${skills().length} skills)`);
      return 0;
    }
    for (const problem of problems) console.error(problem);
    console.error('\nrun `node tools/sync-skills.mjs`');
    return 1;
  }
  console.log(`wrote ${written} file(s) into ${TARGETS.join(' and ')}`);
  return 0;
}

process.exit(main());

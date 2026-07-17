#!/usr/bin/env node
'use strict';

// Publish preflight. Runs from `prepublishOnly`, so it gates `npm publish` and
// nothing else — it is never shipped to, or run by, an installing user.
//
// It exists because the two ways this package can be born broken are both silent
// and both land on the USER, at which point the tarball is immutable and npm
// does not allow a re-publish of the same version:
//
//   1. A tarball missing a platform's binary. `install` then reports a packaging
//      bug to whoever happens to be on that platform (cli.js `resolveBinary`),
//      and every other platform's install passes, so CI looks green.
//   2. The 0.0.0 placeholder version escaping. package.json carries 0.0.0 in git
//      ON PURPOSE — the git tag is the sole source of truth for the version
//      (release-plz.toml `git_only`), and the release workflow stamps it in.
//      Publishing the placeholder would burn the 0.0.0 version forever.
//
// The lists it checks are IMPORTED from cli.js rather than restated, so "what the
// installer looks for" and "what the publisher must ship" are one fact.

const fs = require('node:fs');
const path = require('node:path');

const { SHIPPED_BIN_NAME, BIN_ROOT, SUPPORTED_PLATFORMS } = require('../cli.js');

// The in-git placeholder. Matches package.json's committed `version`.
const PLACEHOLDER_VERSION = '0.0.0';

// A real snapback binary is ~1.8M. This is not a size assertion — it is a
// zero-byte/truncated-file tripwire, set far below any plausible real binary so
// it can only fire on something that is obviously not one.
const MIN_PLAUSIBLE_BIN_BYTES = 100 * 1024;

/** Collect every reason this tree must not be published. */
function findProblems(pkgVersion) {
  const problems = [];

  if (pkgVersion === PLACEHOLDER_VERSION) {
    problems.push(
      `version is still the ${PLACEHOLDER_VERSION} placeholder — it must be stamped from the ` +
        `git tag (see .github/workflows/npm-release.yml)`
    );
  }

  for (const key of SUPPORTED_PLATFORMS) {
    const binPath = path.join(BIN_ROOT, key, SHIPPED_BIN_NAME);
    let stat;
    try {
      stat = fs.statSync(binPath);
    } catch {
      problems.push(`missing binary: ${path.relative(process.cwd(), binPath)}`);
      continue;
    }
    if (stat.size < MIN_PLAUSIBLE_BIN_BYTES) {
      problems.push(
        `implausibly small binary (${stat.size}B): ${path.relative(process.cwd(), binPath)}`
      );
    }
  }

  return problems;
}

function main() {
  const { version } = require('../package.json');
  const problems = findProblems(version);

  if (problems.length > 0) {
    console.error('Refusing to publish snapback-tui:\n');
    for (const problem of problems) console.error(`  - ${problem}`);
    console.error('');
    process.exit(1);
  }

  console.log(`preflight ok: v${version}, ${SUPPORTED_PLATFORMS.length} platform binaries`);
}

if (require.main === module) {
  main();
}

module.exports = { findProblems, PLACEHOLDER_VERSION };

#!/usr/bin/env node
// Runs the rune binary of this package's version, downloading and checking it
// on first use. No dependencies and no install scripts: the archive is fetched
// on the first run, checked against the release's SHA256SUMS, and kept in a
// per-version cache. The download code is shared with the Node binding.
'use strict';

const { spawnSync } = require('node:child_process');

const VERSION = require('../package.json').version;

function fail(message) {
  process.stderr.write(`rune: ${message}\n`);
  process.exit(1);
}

async function main() {
  let binary = process.env.RUNE_BIN;
  if (!binary) {
    // An ES module, so it is imported rather than required.
    const download = await import('./download.mjs');
    const target = download.targetFor();
    if (!target) {
      fail(`no release for ${process.platform} ${process.arch}; see https://github.com/AadiXC0DE/Rune#install`);
    }
    try {
      binary = await download.ensureBinary(VERSION, target);
    } catch (error) {
      fail(error.message);
    }
  }
  const run = spawnSync(binary, process.argv.slice(2), { stdio: 'inherit' });
  if (run.error) {
    fail(`could not start ${binary}: ${run.error.message}`);
  }
  process.exit(run.status === null ? 1 : run.status);
}

main().catch((error) => fail(error.message));

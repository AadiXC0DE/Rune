#!/usr/bin/env node
// Runs the rune binary of this package's version, downloading and checking it
// on first use. No dependencies and no install scripts: the archive is fetched
// on the first run, checked against the release's SHA256SUMS, and kept in a
// per-version cache.
'use strict';

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const VERSION = require('../package.json').version;
const BASE_URL = process.env.RUNE_NPM_BASE_URL || 'https://github.com/AadiXC0DE/Rune/releases';

const TARGETS = {
  'darwin arm64': 'aarch64-apple-darwin',
  'darwin x64': 'x86_64-apple-darwin',
  'linux x64': 'x86_64-unknown-linux-musl',
  'linux arm64': 'aarch64-unknown-linux-musl',
  'win32 x64': 'x86_64-pc-windows-msvc',
};

function fail(message) {
  process.stderr.write(`rune: ${message}\n`);
  process.exit(1);
}

function cacheDirectory() {
  if (process.platform === 'win32') {
    const local = process.env.LOCALAPPDATA || path.join(os.homedir(), 'AppData', 'Local');
    return path.join(local, 'rune-npm', VERSION);
  }
  const cache = process.env.XDG_CACHE_HOME || path.join(os.homedir(), '.cache');
  return path.join(cache, 'rune-npm', VERSION);
}

async function download(url) {
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(`${url} answered ${response.status}`);
  }
  return Buffer.from(await response.arrayBuffer());
}

async function install(target, directory, binary) {
  const windows = target.endsWith('-windows-msvc');
  const archive = `rune-${VERSION}-${target}.${windows ? 'zip' : 'tar.gz'}`;
  const release = `${BASE_URL}/download/v${VERSION}`;
  let bytes;
  let sums;
  try {
    bytes = await download(`${release}/${archive}`);
    sums = (await download(`${release}/SHA256SUMS`)).toString('utf8');
  } catch (error) {
    fail(`could not download ${archive}: ${error.message}`);
  }
  const expected = sums
    .split('\n')
    .map((line) => line.trim().split(/\s+/))
    .find((fields) => fields.length === 2 && fields[1].replace(/^\*/, '') === archive);
  const actual = crypto.createHash('sha256').update(bytes).digest('hex');
  if (!expected || expected[0].toLowerCase() !== actual) {
    fail(`checksum mismatch for ${archive}; nothing was installed`);
  }
  const staging = fs.mkdtempSync(path.join(os.tmpdir(), 'rune-npm-'));
  try {
    const archivePath = path.join(staging, archive);
    fs.writeFileSync(archivePath, bytes);
    const unpacked = path.join(staging, 'unpacked');
    fs.mkdirSync(unpacked);
    const tar = spawnSync(windows ? 'tar.exe' : 'tar', [windows ? '-xf' : '-xzf', archivePath, '-C', unpacked], {
      stdio: 'inherit',
    });
    const name = windows ? 'rune.exe' : 'rune';
    const extracted = path.join(unpacked, name);
    if (tar.status !== 0 || !fs.existsSync(extracted)) {
      fail(`could not unpack ${archive}; nothing was installed`);
    }
    fs.mkdirSync(directory, { recursive: true });
    const placed = `${binary}.new.${process.pid}`;
    fs.copyFileSync(extracted, placed);
    fs.chmodSync(placed, 0o755);
    fs.renameSync(placed, binary);
  } finally {
    fs.rmSync(staging, { recursive: true, force: true });
  }
}

async function main() {
  let binary = process.env.RUNE_BIN;
  if (!binary) {
    const target = TARGETS[`${process.platform} ${process.arch}`];
    if (!target) {
      fail(`no release for ${process.platform} ${process.arch}; see https://github.com/AadiXC0DE/Rune#install`);
    }
    const directory = cacheDirectory();
    binary = path.join(directory, process.platform === 'win32' ? 'rune.exe' : 'rune');
    if (!fs.existsSync(binary)) {
      await install(target, directory, binary);
    }
  }
  const run = spawnSync(binary, process.argv.slice(2), { stdio: 'inherit' });
  if (run.error) {
    fail(`could not start ${binary}: ${run.error.message}`);
  }
  process.exit(run.status === null ? 1 : run.status);
}

main().catch((error) => fail(error.message));

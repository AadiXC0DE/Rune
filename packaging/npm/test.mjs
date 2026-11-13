// The launcher against a local release server: a good digest runs the binary
// it downloaded, a bad digest exits 1 without running anything.
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const launcher = path.join(here, 'bin', 'rune.js');
const version = JSON.parse(fs.readFileSync(path.join(here, 'package.json'), 'utf8')).version;
const targets = {
  'darwin arm64': 'aarch64-apple-darwin',
  'darwin x64': 'x86_64-apple-darwin',
  'linux x64': 'x86_64-unknown-linux-musl',
  'linux arm64': 'aarch64-unknown-linux-musl',
};
const target = targets[`${process.platform} ${process.arch}`];

/// A .tar.gz holding a `rune` script that prints its arguments.
function archive() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'rune-npm-fixture-'));
  fs.writeFileSync(path.join(dir, 'rune'), '#!/bin/sh\necho "fake rune $*"\n', { mode: 0o755 });
  const out = path.join(dir, 'rune.tar.gz');
  const tar = spawnSync('tar', ['-czf', out, '-C', dir, 'rune']);
  assert.equal(tar.status, 0, String(tar.stderr));
  return fs.readFileSync(out);
}

async function serve(honest) {
  const bytes = archive();
  const name = `rune-${version}-${target}.tar.gz`;
  const digest = crypto.createHash('sha256').update(honest ? bytes : Buffer.concat([bytes, Buffer.from('x')])).digest('hex');
  const files = {
    [`/download/v${version}/${name}`]: bytes,
    [`/download/v${version}/SHA256SUMS`]: Buffer.from(`${digest}  ${name}\n`),
  };
  const server = http.createServer((request, response) => {
    const body = files[request.url];
    response.writeHead(body ? 200 : 404);
    response.end(body || '');
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  return server;
}

/// Runs the launcher without blocking, so this process's server can answer it.
function run(server, cache) {
  const child = spawn(process.execPath, [launcher, 'hello', 'there'], {
    env: {
      ...process.env,
      RUNE_BIN: '',
      RUNE_NPM_BASE_URL: `http://127.0.0.1:${server.address().port}`,
      XDG_CACHE_HOME: cache,
    },
  });
  let stdout = '';
  let stderr = '';
  child.stdout.on('data', (chunk) => { stdout += chunk; });
  child.stderr.on('data', (chunk) => { stderr += chunk; });
  return new Promise((resolve) => {
    child.on('close', (status) => resolve({ status, stdout, stderr }));
  });
}

test('a good digest runs the downloaded binary', { skip: !target && 'no release for this platform' }, async () => {
  const server = await serve(true);
  const cache = fs.mkdtempSync(path.join(os.tmpdir(), 'rune-npm-cache-'));
  try {
    const result = await run(server, cache);
    assert.equal(result.status, 0, result.stderr);
    assert.equal(result.stdout.trim(), 'fake rune hello there');
    assert.ok(fs.existsSync(path.join(cache, 'rune-npm', version, 'rune')));
    // A second run starts the cached binary without downloading.
    server.close();
    const again = await run({ address: () => ({ port: 9 }) }, cache);
    assert.equal(again.stdout.trim(), 'fake rune hello there');
  } finally {
    server.close();
  }
});

test('a bad digest exits 1 and runs nothing', { skip: !target && 'no release for this platform' }, async () => {
  const server = await serve(false);
  const cache = fs.mkdtempSync(path.join(os.tmpdir(), 'rune-npm-cache-'));
  try {
    const result = await run(server, cache);
    assert.equal(result.status, 1);
    assert.match(result.stderr, /^rune: checksum mismatch for rune-.*; nothing was installed\n$/);
    assert.ok(!result.stdout.includes('fake rune'));
    assert.ok(!fs.existsSync(path.join(cache, 'rune-npm', version, 'rune')));
  } finally {
    server.close();
  }
});

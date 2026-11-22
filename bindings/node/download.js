// Downloads the rune release archive of one version, checks it against the
// release's SHA256SUMS, and keeps the binary in a per-version cache. Shared by
// the Node binding and the npm launcher; the launcher ships a byte copy as
// bin/download.mjs, and a test keeps the two identical.
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';

/** Release target triple for each `platform arch` pair that has an archive. */
export const TARGETS = {
  'darwin arm64': 'aarch64-apple-darwin',
  'darwin x64': 'x86_64-apple-darwin',
  'linux x64': 'x86_64-unknown-linux-musl',
  'linux arm64': 'aarch64-unknown-linux-musl',
  'win32 x64': 'x86_64-pc-windows-msvc',
};

/** Where releases are downloaded from; `RUNE_NPM_BASE_URL` overrides it for tests. */
export function baseUrl() {
  return process.env.RUNE_NPM_BASE_URL || 'https://github.com/AadiXC0DE/Rune/releases';
}

/** The release target for this machine, or null when no archive is published for it. */
export function targetFor(platform = process.platform, arch = process.arch) {
  return TARGETS[`${platform} ${arch}`] ?? null;
}

/** The cache directory for one version. */
export function cacheDirectory(version) {
  if (process.platform === 'win32') {
    const local = process.env.LOCALAPPDATA || path.join(os.homedir(), 'AppData', 'Local');
    return path.join(local, 'rune-npm', version);
  }
  const cache = process.env.XDG_CACHE_HOME || path.join(os.homedir(), '.cache');
  return path.join(cache, 'rune-npm', version);
}

/** The path the cached binary of a version has, whether or not it exists yet. */
export function cachedBinary(version) {
  return path.join(cacheDirectory(version), process.platform === 'win32' ? 'rune.exe' : 'rune');
}

/**
 * The tar that unpacks a release archive. On Windows that is the system's own
 * `tar.exe`, which reads zip archives; a GNU tar earlier on `PATH`, as Git Bash
 * puts there, reads `C:` as a remote host and cannot read a zip at all.
 */
export function tarProgram(windows = process.platform === 'win32') {
  if (!windows) {
    return 'tar';
  }
  return path.join(process.env.SystemRoot || 'C:\\Windows', 'System32', 'tar.exe');
}

async function fetchBytes(url) {
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(`${url} answered ${response.status}`);
  }
  return Buffer.from(await response.arrayBuffer());
}

/**
 * Returns the cached binary of `version`, downloading and checking it first
 * when it is not cached. Throws when there is no archive for `target`, the
 * download fails, the digest does not match, or the archive cannot be
 * unpacked; nothing is installed in those cases.
 */
export async function ensureBinary(version, target) {
  const binary = cachedBinary(version);
  if (fs.existsSync(binary)) {
    return binary;
  }
  const windows = target.endsWith('-windows-msvc');
  const archive = `rune-${version}-${target}.${windows ? 'zip' : 'tar.gz'}`;
  const release = `${baseUrl()}/download/v${version}`;
  let bytes;
  let sums;
  try {
    bytes = await fetchBytes(`${release}/${archive}`);
    sums = (await fetchBytes(`${release}/SHA256SUMS`)).toString('utf8');
  } catch (error) {
    throw new Error(`could not download ${archive}: ${error.message}`);
  }
  const expected = sums
    .split('\n')
    .map((line) => line.trim().split(/\s+/))
    .find((fields) => fields.length === 2 && fields[1].replace(/^\*/, '') === archive);
  const actual = crypto.createHash('sha256').update(bytes).digest('hex');
  if (!expected || expected[0].toLowerCase() !== actual) {
    throw new Error(`checksum mismatch for ${archive}; nothing was installed`);
  }
  const staging = fs.mkdtempSync(path.join(os.tmpdir(), 'rune-npm-'));
  try {
    const archivePath = path.join(staging, archive);
    fs.writeFileSync(archivePath, bytes);
    const unpacked = path.join(staging, 'unpacked');
    fs.mkdirSync(unpacked);
    const tar = spawnSync(tarProgram(windows), [windows ? '-xf' : '-xzf', archivePath, '-C', unpacked], {
      stdio: 'inherit',
    });
    const extracted = path.join(unpacked, windows ? 'rune.exe' : 'rune');
    if (tar.status !== 0 || !fs.existsSync(extracted)) {
      throw new Error(`could not unpack ${archive}; nothing was installed`);
    }
    fs.mkdirSync(path.dirname(binary), { recursive: true });
    const placed = `${binary}.new.${process.pid}`;
    fs.copyFileSync(extracted, placed);
    fs.chmodSync(placed, 0o755);
    fs.renameSync(placed, binary);
  } finally {
    fs.rmSync(staging, { recursive: true, force: true });
  }
  return binary;
}

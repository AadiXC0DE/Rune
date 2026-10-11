import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, join } from "node:path";
import { test } from "node:test";

import { resolveBinary, RuneBinaryNotFoundError } from "./index.js";

// Use real fixture files on every host. On Windows this uses the native
// platform; elsewhere it exercises the Windows branch without spawning.
function withFixture(platform, run) {
  const directory = mkdtempSync(join(tmpdir(), "rune-binary-"));
  const savedPlatform = Object.getOwnPropertyDescriptor(process, "platform");
  const savedPath = process.env.PATH;
  const savedBinary = process.env.RUNE_BIN;
  try {
    Object.defineProperty(process, "platform", { ...savedPlatform, value: platform });
    process.env.PATH = directory;
    delete process.env.RUNE_BIN;
    run(directory);
  } finally {
    Object.defineProperty(process, "platform", savedPlatform);
    if (savedPath === undefined) delete process.env.PATH;
    else process.env.PATH = savedPath;
    if (savedBinary === undefined) delete process.env.RUNE_BIN;
    else process.env.RUNE_BIN = savedBinary;
    rmSync(directory, { recursive: true, force: true });
  }
}

test("Windows PATH lookup finds a fixture containing only rune.exe", () => {
  withFixture("win32", (directory) => {
    const executable = join(directory, "rune.exe");
    writeFileSync(executable, "fixture", { mode: 0o755 });

    assert.equal(resolveBinary(), executable);
    assert.equal(resolveBinary({ bin: "rune" }), executable);
    process.env.RUNE_BIN = "rune";
    assert.equal(resolveBinary(), executable);
    assert.equal(resolveBinary({ bin: "rune.exe" }), executable);
  });
});

test("Windows PATH lookup preserves directory order and exact-name precedence", () => {
  withFixture("win32", (directory) => {
    const later = join(directory, "later");
    mkdirSync(later);
    process.env.PATH = [directory, later].join(delimiter);
    const executable = join(directory, "rune.exe");
    writeFileSync(executable, "fixture", { mode: 0o755 });
    writeFileSync(join(later, "rune"), "fixture", { mode: 0o755 });
    assert.equal(resolveBinary(), executable);

    const exact = join(directory, "rune");
    writeFileSync(exact, "fixture", { mode: 0o755 });
    assert.equal(resolveBinary(), exact);
  });
});

test("Windows PATH lookup rejects a directory named rune.exe", () => {
  withFixture("win32", (directory) => {
    mkdirSync(join(directory, "rune.exe"));
    assert.throws(() => resolveBinary(), RuneBinaryNotFoundError);
  });
});

test("Unix PATH lookup does not infer an .exe suffix", () => {
  withFixture("linux", (directory) => {
    writeFileSync(join(directory, "rune.exe"), "fixture", { mode: 0o755 });
    assert.throws(() => resolveBinary(), RuneBinaryNotFoundError);

    const executable = join(directory, "rune");
    writeFileSync(executable, "fixture", { mode: 0o755 });
    assert.equal(resolveBinary(), executable);
  });
});

import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { WASI } from 'node:wasi';

// Require the freshly built artifact explicitly, never the committed demo module.
assert.equal(process.argv.length, 3, 'usage: node .github/scripts/test-wasm.mjs <rune_web.wasm>');
const wasmPath = process.argv[2];

async function fixturePrompt(readLength, expectedError) {
  const workspace = await mkdtemp(join(tmpdir(), 'rune-wasm-smoke-'));
  try {
    const wasi = new WASI({
      version: 'preview1',
      args: [],
      env: {},
      preopens: { '/workspace': workspace },
    });
    const prompt = 'Reply to the WASM fixture: café 🦀';
    const answer = 'WASM fixture completed: café 🦀';
    const events = [];
    let requests = 0;
    let reads = 0;
    let closed = false;
    let staged;
    let memory;
    let offset = 0;
    const frame = (delta, finish_reason) => `data: ${JSON.stringify({
      id: 'wasm-fixture',
      object: 'chat.completion.chunk',
      model: 'wasm-fixture-model',
      choices: [{ index: 0, delta, finish_reason }],
    })}\n\n`;
    const body = Buffer.from(
      frame({ content: answer }, null) + frame({}, 'stop') + 'data: [DONE]\n\n',
    );
    // Recreate views after allocations, since WASM may grow its memory.
    const bytes = () => new Uint8Array(memory.buffer);
    const decode = (pointer, length) => Buffer.from(bytes().subarray(pointer, pointer + length)).toString('utf8');
    const imports = {
      wasi_snapshot_preview1: wasi.wasiImport,
      rune: {
        emit(pointer, length) {
          events.push(JSON.parse(decode(pointer, length)));
        },
        open(pointer, length) {
          assert.equal(++requests, 1, 'the fixture expects exactly one provider request');
          const request = JSON.parse(decode(pointer, length));
          assert.equal(request.method, 'POST');
          assert.equal(request.url, 'https://wasm-fixture.invalid/v1/chat/completions');
          assert.ok(request.headers.some(([name, value]) => name.toLowerCase() === 'authorization'
            && value === 'Bearer wasm-fixture-key'));
          const payload = JSON.parse(request.body);
          assert.equal(payload.model, 'wasm-fixture-model');
          assert.equal(payload.stream, true);
          assert.deepEqual(payload.messages.at(-1), { role: 'user', content: prompt });
          staged = Buffer.from(JSON.stringify({ handle: 1, status: 200, content_type: 'text/event-stream' }));
          return staged.length;
        },
        take(pointer, length) {
          assert.ok(staged, 'open must stage a response head');
          assert.equal(length, staged.length);
          bytes().set(staged, pointer);
          staged = undefined;
        },
        read(handle, pointer, capacity) {
          assert.equal(handle, 1);
          assert.equal(closed, false);
          reads++;
          if (readLength) {
            assert.equal(reads, 1, 'an invalid body length must not reach a second read');
            return readLength(capacity);
          }
          // Split SSE frames and multibyte text across reads.
          const count = Math.min(capacity, 7, body.length - offset);
          bytes().set(body.subarray(offset, offset + count), pointer);
          offset += count;
          return count;
        },
        close(handle) {
          assert.equal(handle, 1);
          assert.equal(closed, false);
          closed = true;
        },
        ask() { assert.fail('the fixture must not ask for tool permission'); },
        run() { assert.fail('the fixture must not run a shell command'); },
      },
    };
    const { instance } = await WebAssembly.instantiate(await readFile(wasmPath), imports);
    const exports = instance.exports;
    memory = exports.memory;
    for (const name of ['rune_alloc', 'rune_configure', 'rune_prompt']) {
      assert.equal(typeof exports[name], 'function', `missing export ${name}`);
    }
    wasi.initialize(instance);
    const pass = (text) => {
      const encoded = Buffer.from(text);
      const pointer = exports.rune_alloc(encoded.length);
      bytes().set(encoded, pointer);
      // The called export takes ownership of this allocation.
      return [pointer, encoded.length];
    };
    assert.equal(exports.rune_configure(...pass(JSON.stringify({
      dialect: 'chat_completions',
      base_url: 'https://wasm-fixture.invalid/v1',
      model: 'wasm-fixture-model',
      api_key: 'wasm-fixture-key',
      workspace: '/workspace',
      max_steps: 1,
    }))), 0, `configuration failed: ${JSON.stringify(events)}`);
    assert.equal(exports.rune_prompt(...pass(prompt)), expectedError ? 1 : 0,
      `unexpected prompt result: ${JSON.stringify(events)}`);
    assert.equal(requests, 1);
    assert.equal(closed, true, 'the response handle must be released');
    assert.equal(staged, undefined, 'the response head must cross the take import');
    if (expectedError) {
      assert.equal(reads, 1, 'an invalid body length must stop the first read');
      const errors = events.filter((event) => event.kind === 'error');
      assert.equal(errors.length, 1);
      assert.equal(errors[0].code, 'transport_failure');
      assert.ok(errors[0].message.includes(expectedError), JSON.stringify(errors[0]));
      assert.deepEqual(events.filter((event) => event.kind === 'text'), []);
      assert.deepEqual(events.filter((event) => event.kind === 'finished'), []);
      return;
    }
    assert.ok(reads > 1, 'the response must stream through the read import');
    assert.deepEqual(events.filter((event) => event.kind === 'error'), []);
    assert.equal(events.filter((event) => event.kind === 'text').map((event) => event.delta).join(''), answer);
    const finished = events.filter((event) => event.kind === 'finished');
    assert.equal(finished.length, 1);
    assert.equal(finished[0].completed, true);
    assert.equal(finished[0].reason, 'end_turn');
    assert.equal(finished[0].steps, 1);
  } finally {
    await rm(workspace, { recursive: true, force: true });
  }
}

test('configured fixture prompt crosses WASM exports and host imports', () => fixturePrompt());
test('WASM rejects a body length one byte above capacity before slicing', () => fixturePrompt(
  (capacity) => capacity + 1, 'the page returned a body length larger than the buffer capacity',
));
test('WASM rejects the maximum positive imported body length before slicing', () => fixturePrompt(
  () => 0x7fffffff, 'the page returned a body length larger than the buffer capacity',
));
test('WASM preserves the error for negative imported body lengths', () => fixturePrompt(
  () => -1, 'the page ended the response',
));

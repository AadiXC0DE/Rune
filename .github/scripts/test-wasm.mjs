import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { WASI } from 'node:wasi';

// Require the freshly built artifact explicitly, never the committed demo module.
assert.equal(process.argv.length, 3, 'usage: node .github/scripts/test-wasm.mjs <rune_web.wasm>');
const wasmPath = process.argv[2];

async function fixturePrompt(readLength, expectedError, staging = {}) {
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
    let takes = 0;
    let runs = 0;
    let memoryAtStage;
    const shell = staging.target === 'run';
    const frame = (delta, finish_reason) => `data: ${JSON.stringify({
      id: 'wasm-fixture',
      object: 'chat.completion.chunk',
      model: 'wasm-fixture-model',
      choices: [{ index: 0, delta, finish_reason }],
    })}\n\n`;
    const body = Buffer.from(
      (shell
        ? frame({ tool_calls: [{ index: 0, id: 'shell-fixture', type: 'function',
          function: { name: 'shell', arguments: JSON.stringify({ command: 'ls' }) } }] }, null)
          + frame({}, 'tool_calls')
        : frame({ content: answer }, null) + frame({}, 'stop')) + 'data: [DONE]\n\n',
    );
    // Recreate views after allocations, since WASM may grow its memory.
    const bytes = () => new Uint8Array(memory.buffer);
    const decode = (pointer, length) => Buffer.from(bytes().subarray(pointer, pointer + length)).toString('utf8');
    const stage = (value, target) => {
      staged = Buffer.from(JSON.stringify(value));
      if (target === (staging.target ?? 'open') && staging.length !== undefined) {
        memoryAtStage = memory.buffer.byteLength;
        if (staging.reject) return staging.length;
        assert.ok(staged.length <= staging.length);
        // JSON whitespace pads a valid reply to exactly the configured cap.
        staged = Buffer.from(staged.toString().padEnd(staging.length, ' '));
      }
      return staged.length;
    };
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
          return stage({ handle: 1, status: 200, content_type: 'text/event-stream' }, 'open');
        },
        take(pointer, length) {
          takes++;
          assert.ok(staged, 'a host import must stage a reply');
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
        run(pointer, length) {
          assert.equal(shell, true, 'the fixture must not run a shell command');
          assert.equal(++runs, 1);
          assert.deepEqual(JSON.parse(decode(pointer, length)), { command: 'ls', cwd: '/workspace' });
          return stage({ output: 'shell fixture completed', exit_code: 0 }, 'run');
        },
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
      ...(staging.cap === undefined ? {} : { max_bridge_bytes: staging.cap }),
    }))), 0, `configuration failed: ${JSON.stringify(events)}`);
    assert.equal(exports.rune_prompt(...pass(prompt)), expectedError && !shell ? 1 : 0,
      `unexpected prompt result: ${JSON.stringify(events)}`);
    assert.equal(requests, 1);
    if (staging.reject) {
      assert.equal(memory.buffer.byteLength, memoryAtStage,
        'a rejected staged length must not grow WASM memory');
      assert.equal(takes, shell ? 1 : 0, 'a rejected reply must not reach take');
      assert.ok(staged, 'the rejected reply must remain on the host');
      if (!shell) {
        assert.equal(reads, 0, 'a rejected head must not reach body reads');
        assert.equal(closed, false, 'no response handle was accepted');
        const errors = events.filter((event) => event.kind === 'error');
        assert.equal(errors.length, 1);
        assert.equal(errors[0].code, 'transport_failure');
        assert.ok(errors[0].message.includes(expectedError), JSON.stringify(errors[0]));
        assert.deepEqual(events.filter((event) => event.kind === 'text'), []);
        assert.deepEqual(events.filter((event) => event.kind === 'finished'), []);
        return;
      }
    }
    assert.equal(closed, true, 'the response handle must be released');
    if (!staging.reject) assert.equal(staged, undefined, 'accepted replies must cross the take import');
    if (shell) {
      assert.equal(runs, 1);
      const outputs = events.filter((event) => event.kind === 'tool_output');
      assert.equal(outputs.length, 1);
      assert.equal(outputs[0].is_error, Boolean(staging.reject));
      assert.ok(outputs[0].headline.includes(staging.reject ? expectedError : 'shell fixture completed'),
        JSON.stringify(outputs[0]));
      const finishedTools = events.filter((event) => event.kind === 'tool_finished');
      assert.equal(finishedTools.length, 1);
      assert.equal(finishedTools[0].is_error, Boolean(staging.reject));
      assert.deepEqual(events.filter((event) => event.kind === 'error'), []);
      assert.equal(events.filter((event) => event.kind === 'finished').length, 1);
      return;
    }
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

for (const target of ['open', 'run']) {
  for (const [name, cap, length] of [
    ['configured cap plus one', 256, 257],
    ['default cap plus one', undefined, 1024 * 1024 + 1],
    ['maximum positive host length', 256, 0x7fffffff],
    ['zero cap', 0, 1],
  ]) {
    // Shell cases need room for the response head before reaching run.
    if (target === 'run' && cap === 0) continue;
    test(`WASM ${target} rejects ${name} without taking the reply or growing memory`, () => fixturePrompt(
      undefined, 'larger than the bridge byte cap', { target, cap, length, reject: true },
    ));
  }
  test(`WASM ${target} accepts a staged reply exactly at the configured cap`, () => fixturePrompt(
    undefined, undefined, { target, cap: 256, length: 256 },
  ));
  test(`WASM ${target} preserves the error for a negative staged length`, () => fixturePrompt(
    undefined, target === 'open' ? 'the page sent no reply' : "the page's shell did not answer",
    { target, cap: 256, length: -1, reject: true },
  ));
}

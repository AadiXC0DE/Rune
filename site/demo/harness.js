// Loads the harness, compiled to WebAssembly, and gives it what a tab has in
// place of a machine: an in-memory workspace mounted through WASI, requests
// made with fetch, a small shell over the same files, and a person to ask.
//
// The imports that wait are wrapped in WebAssembly.Suspending and the prompt
// export in WebAssembly.promising, so the Rust loop reads as blocking code
// while the tab keeps running. That is JavaScript Promise Integration, which
// every current engine ships.

import { WASI, File, Directory, PreopenDirectory, OpenFile, ConsoleStdout } from './wasi.js';

const encoder = new TextEncoder();
const decoder = new TextDecoder();

export const WORKSPACE_ROOT = '/workspace';

export function supported() {
  return typeof WebAssembly === 'object'
    && typeof WebAssembly.Suspending === 'function'
    && typeof WebAssembly.promising === 'function';
}

// ---------- the workspace ----------

export function tree(files) {
  const root = new Map();
  for (const [path, text] of Object.entries(files)) {
    const parts = path.split('/');
    let dir = root;
    for (const part of parts.slice(0, -1)) {
      if (!dir.has(part)) dir.set(part, new Directory(new Map()));
      dir = dir.get(part).contents;
    }
    dir.set(parts[parts.length - 1], new File(encoder.encode(text)));
  }
  return root;
}

// Walks the in-memory tree, yielding [path, inode] for every entry.
function* walk(contents, prefix = '') {
  const names = [...contents.keys()].sort();
  for (const name of names) {
    const node = contents.get(name);
    const path = prefix ? prefix + '/' + name : name;
    yield [path, node];
    if (node instanceof Directory) yield* walk(node.contents, path);
  }
}

function lookup(contents, path) {
  const parts = path.split('/').filter((part) => part && part !== '.');
  let node = { contents };
  for (const part of parts) {
    if (!(node.contents instanceof Map) || !node.contents.has(part)) return null;
    node = node.contents.get(part);
  }
  return node;
}

// ---------- the page's shell ----------
//
// One command at a time, over the workspace files. It exists so a model that
// reaches for `ls` or `cat` gets an answer, and so the permission rules have
// something real to decide about. A compiler or the network is out of reach
// of a tab, and the shell says so rather than pretending.

function words(line) {
  const out = [];
  const pattern = /"([^"]*)"|'([^']*)'|(\S+)/g;
  let match;
  while ((match = pattern.exec(line))) out.push(match[1] ?? match[2] ?? match[3]);
  return out;
}

function globToRegExp(glob) {
  let source = '';
  for (const c of glob) {
    if (c === '*') source += '[^/]*';
    else if (c === '?') source += '[^/]';
    else source += c.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  }
  return new RegExp('^' + source + '$');
}

export function makeShell(root) {
  const text = (node) => decoder.decode(node.data);
  const clean = (path) => (path || '.').replace(/^\.\/?/, '').replace(/^\/workspace\/?/, '').replace(/\/$/, '');

  const commands = {
    pwd: () => [WORKSPACE_ROOT, 0],
    echo: (args) => [args.join(' '), 0],
    ls: (args) => {
      const target = clean(args.find((a) => !a.startsWith('-')));
      const node = target ? lookup(root, target) : { contents: root };
      if (!node) return [`ls: ${target}: No such file or directory`, 1];
      if (!(node.contents instanceof Map)) return [target, 0];
      const names = [...node.contents.keys()].sort()
        .map((name) => node.contents.get(name) instanceof Directory ? name + '/' : name);
      return [names.join('\n'), 0];
    },
    tree: (args) => {
      const target = clean(args[0]);
      const node = target ? lookup(root, target) : { contents: root };
      if (!node || !(node.contents instanceof Map)) return [`tree: ${target}: not a directory`, 1];
      const lines = ['.'];
      for (const [path, entry] of walk(node.contents)) {
        const depth = path.split('/').length - 1;
        const name = path.split('/').pop();
        lines.push('  '.repeat(depth) + (entry instanceof Directory ? name + '/' : name));
      }
      return [lines.join('\n'), 0];
    },
    cat: (args) => {
      const out = [];
      for (const arg of args.filter((a) => !a.startsWith('-'))) {
        const node = lookup(root, clean(arg));
        if (!node || node.contents instanceof Map) return [`cat: ${arg}: No such file`, 1];
        out.push(text(node).replace(/\n$/, ''));
      }
      return [out.join('\n'), 0];
    },
    head: (args, tail = false) => {
      let count = 10;
      const files = [];
      for (let i = 0; i < args.length; i++) {
        if (args[i] === '-n') count = parseInt(args[++i], 10) || 10;
        else if (/^-\d+$/.test(args[i])) count = parseInt(args[i].slice(1), 10);
        else files.push(args[i]);
      }
      const node = lookup(root, clean(files[0]));
      if (!node || node.contents instanceof Map) return [`${tail ? 'tail' : 'head'}: ${files[0]}: No such file`, 1];
      const lines = text(node).replace(/\n$/, '').split('\n');
      return [(tail ? lines.slice(-count) : lines.slice(0, count)).join('\n'), 0];
    },
    tail: (args) => commands.head(args, true),
    wc: (args) => {
      const flag = args.find((a) => a.startsWith('-')) || '';
      const out = [];
      for (const arg of args.filter((a) => !a.startsWith('-'))) {
        const node = lookup(root, clean(arg));
        if (!node || node.contents instanceof Map) return [`wc: ${arg}: No such file`, 1];
        const body = text(node);
        const lines = (body.match(/\n/g) || []).length;
        const count = body.split(/\s+/).filter(Boolean).length;
        out.push(flag === '-l' ? `${lines} ${arg}` : flag === '-w' ? `${count} ${arg}` : `${lines} ${count} ${node.data.length} ${arg}`);
      }
      return [out.join('\n'), 0];
    },
    grep: (args) => {
      const flags = args.filter((a) => a.startsWith('-')).join('');
      const rest = args.filter((a) => !a.startsWith('-'));
      const [pattern, ...targets] = rest;
      if (!pattern) return ['usage: grep [-rin] pattern [path]', 2];
      let test;
      try { test = new RegExp(pattern, flags.includes('i') ? 'i' : ''); }
      catch { test = { test: (line) => line.includes(pattern) }; }
      const hits = [];
      const scan = (path, node) => {
        text(node).split('\n').forEach((line, index) => {
          if (test.test(line)) hits.push(`${path}:${flags.includes('n') || targets.length !== 1 ? index + 1 + ':' : ''}${line}`);
        });
      };
      const roots = targets.length ? targets.map(clean) : [''];
      for (const target of roots) {
        const node = target ? lookup(root, target) : { contents: root };
        if (!node) return [`grep: ${target}: No such file or directory`, 2];
        if (node.contents instanceof Map) {
          for (const [path, entry] of walk(node.contents, target)) if (entry instanceof File) scan(path, entry);
        } else scan(target, node);
      }
      return [hits.join('\n'), hits.length ? 0 : 1];
    },
    find: (args) => {
      const start = clean(args[0] && !args[0].startsWith('-') ? args[0] : '');
      const nameAt = args.indexOf('-name');
      const test = nameAt >= 0 ? globToRegExp(args[nameAt + 1] || '*') : null;
      const node = start ? lookup(root, start) : { contents: root };
      if (!node || !(node.contents instanceof Map)) return [`find: ${start}: No such directory`, 1];
      const out = [];
      for (const [path] of walk(node.contents, start || '.')) {
        if (!test || test.test(path.split('/').pop())) out.push(path);
      }
      return [out.join('\n'), 0];
    },
  };

  // One command. Pipes and redirection have nothing to connect to here, so
  // they are refused with a reason rather than run halfway.
  function one(line) {
    const trimmed = line.trim();
    if (!trimmed) return ['', 0];
    if (/[|><`$]/.test(trimmed)) {
      return ['this shell has no pipes or redirection; run the commands one at a time', 2];
    }
    const [name, ...args] = words(trimmed);
    if (name === 'cd') {
      const target = clean(args[0] || '');
      const node = target ? lookup(root, target) : { contents: root };
      return node && node.contents instanceof Map ? ['', 0] : [`cd: ${args[0]}: No such directory`, 1];
    }
    if (name === 'git') return ['git: this tab has no repository history', 127];
    const command = commands[name];
    if (!command) {
      return [
        `${name}: a browser tab cannot start programs. The installed binary runs it in a sandbox.\n` +
          `Do not run ${name} again here; answer from the files you have read.`,
        127,
      ];
    }
    return command(args);
  }

  // `a && b` stops at the first failure and `a; b` does not, as a shell does.
  return function run(line) {
    const out = [];
    let code = 0;
    for (const part of line.split(/(&&|;)/)) {
      if (part === '&&') {
        if (code !== 0) break;
        continue;
      }
      if (part === ';') continue;
      const [text, status] = one(part);
      if (text) out.push(text);
      code = status;
    }
    return [out.join('\n'), code];
  };
}

// ---------- the harness ----------

export async function createHarness({ wasmUrl, files, transport, onEvent, onAsk }) {
  const root = tree(files);
  const original = { ...files };
  const wasi = new WASI([], [], [
    new OpenFile(new File([])),
    ConsoleStdout.lineBuffered((line) => console.log('[rune]', line)),
    ConsoleStdout.lineBuffered((line) => console.warn('[rune]', line)),
    new PreopenDirectory(WORKSPACE_ROOT, root),
  ], { debug: false });
  const shell = makeShell(root);

  let memory;
  let staged = null;
  const bodies = new Map();
  let nextHandle = 1;

  const view = () => new Uint8Array(memory.buffer);
  const read = (pointer, length) => decoder.decode(view().slice(pointer, pointer + length));
  const stage = (value) => {
    staged = encoder.encode(JSON.stringify(value));
    return staged.length;
  };

  const imports = {
    wasi_snapshot_preview1: wasi.wasiImport,
    rune: {
      emit(pointer, length) {
        try { onEvent(JSON.parse(read(pointer, length))); }
        catch (err) { console.error(err); }
      },
      take(pointer, length) {
        view().set(staged.subarray(0, length), pointer);
        staged = null;
      },
      open: new WebAssembly.Suspending(async (pointer, length) => {
        const request = JSON.parse(read(pointer, length));
        try {
          const response = await transport(request);
          const handle = nextHandle++;
          bodies.set(handle, { reader: response.body, pending: new Uint8Array(0), done: false });
          return stage({ handle, status: response.status, content_type: response.contentType || '' });
        } catch (err) {
          return stage({ error: err && err.message ? err.message : String(err) });
        }
      }),
      read: new WebAssembly.Suspending(async (handle, pointer, capacity) => {
        const body = bodies.get(handle);
        if (!body) return -1;
        try {
          while (body.pending.length === 0) {
            if (body.done) return 0;
            const { value, done } = await body.reader.read();
            if (done) body.done = true;
            else body.pending = typeof value === 'string' ? encoder.encode(value) : value;
          }
        } catch {
          return -1;
        }
        const count = Math.min(capacity, body.pending.length);
        view().set(body.pending.subarray(0, count), pointer);
        body.pending = body.pending.subarray(count);
        return count;
      }),
      close(handle) {
        const body = bodies.get(handle);
        bodies.delete(handle);
        if (body && !body.done && body.reader.cancel) body.reader.cancel().catch(() => {});
      },
      ask: new WebAssembly.Suspending(async (pointer, length) => {
        const allowed = await onAsk(JSON.parse(read(pointer, length)));
        return allowed ? 1 : 0;
      }),
      run: new WebAssembly.Suspending(async (pointer, length) => {
        const { command } = JSON.parse(read(pointer, length));
        const [output, exit_code] = shell(command);
        return stage({ output, exit_code });
      }),
    },
  };

  const { instance } = await WebAssembly.instantiateStreaming(fetch(wasmUrl), imports);
  memory = instance.exports.memory;
  wasi.initialize({ exports: { memory, _initialize: instance.exports._initialize } });

  const exports = instance.exports;
  const promptExport = WebAssembly.promising(exports.rune_prompt);

  const pass = (textValue) => {
    const bytes = encoder.encode(textValue);
    const pointer = exports.rune_alloc(bytes.length);
    view().set(bytes, pointer);
    return [pointer, bytes.length];
  };

  return {
    configure(config) {
      return exports.rune_configure(...pass(JSON.stringify(config))) === 0;
    },
    async prompt(textValue) {
      return (await promptExport(...pass(textValue))) === 0;
    },
    cancel() {
      exports.rune_cancel();
    },
    steer(textValue) {
      return exports.rune_steer(...pass(textValue)) === 0;
    },
    reset() {
      exports.rune_reset();
    },
    // The files as they are now, for /cat, /files and /diff.
    files() {
      const out = {};
      for (const [path, node] of walk(root)) if (node instanceof File) out[path] = decoder.decode(node.data);
      return out;
    },
    original() {
      return original;
    },
    // Puts every file back the way the page shipped it.
    restore() {
      root.clear();
      for (const [name, node] of tree(original)) root.set(name, node);
    },
    shell,
  };
}

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import vm from 'node:vm';

const moduleUrl = new URL('./demo.js', import.meta.url);
const source = await readFile(moduleUrl, 'utf8');

// A small DOM/event fixture exercises mount's actual handlers and onAsk promise.
// Network/model/WASM boundaries are mocked; permission decisions and composer
// routing are the production demo code.
async function fixture() {
  const created = [];
  let document;
  class Element {
    constructor(tag = 'div') {
      this.tag = tag;
      this.children = [];
      this.listeners = new Map();
      this.value = '';
      this.textContent = '';
      this.hidden = true;
      this.classes = new Set();
      this.classList = {
        add: (name) => this.classes.add(name),
        remove: (name) => this.classes.delete(name),
        contains: (name) => this.classes.has(name),
        toggle: (name, on) => on ? this.classes.add(name) : this.classes.delete(name),
      };
      created.push(this);
    }
    addEventListener(type, callback, capture = false) {
      const handlers = this.listeners.get(type) || [];
      handlers.push({ callback, capture });
      this.listeners.set(type, handlers);
    }
    fire(type, event = {}) {
      for (const { callback } of this.listeners.get(type) || []) callback(event);
    }
    append(...children) {
      this.children.push(...children);
      for (const child of children) if (child instanceof Element) child.parentNode = this;
    }
    replaceChildren(...children) {
      for (const child of [...this.children]) if (child instanceof Element) child.remove();
      this.children = [];
      this.append(...children);
    }
    remove() {
      if (this.parentNode) {
        this.parentNode.children = this.parentNode.children.filter((child) => child !== this);
        this.parentNode = null;
      }
    }
    focus() {
      document.activeElement = this;
      this.fire('focus');
    }
    setAttribute(name, value) { this[name] = value; }
    querySelector(selector) { return this.selectors.get(selector); }
  }
  document = new Element('document');
  document.body = new Element('body');
  document.activeElement = document.body;
  document.createElement = (tag) => new Element(tag);
  const root = new Element();
  root.selectors = new Map(['screen', 'input', 'input-row', 'status', 'hint', 'model', 'panel']
    .map((name) => [`[data-${name}]`, new Element(name)]));
  const input = root.querySelector('[data-input]');
  const panel = root.querySelector('[data-panel]');
  const form = new Element('form');
  form.elements = Object.fromEntries(['provider', 'base_url', 'model', 'key']
    .map((name) => [name, new Element('input')]));
  form.elements.provider.value = 'fixture';
  panel.selectors = new Map([
    ['[data-use-scripted]', new Element('button')],
    ['[data-endpoint]', form],
    ['[data-use-local]', new Element('button')],
  ]);
  const answers = [];
  const steered = [];
  const prompts = [];
  let callbacks;
  const harness = {
    configure: () => true,
    async prompt(text) {
      prompts.push(text);
      answers.push(await callbacks.onAsk({ tool: 'shell', target: 'cargo test' }));
    },
    steer: (text) => { steered.push(text); return true; },
  };
  const context = vm.createContext({
    document, URL, performance, setTimeout, clearTimeout,
    window: { getSelection: () => '', matchMedia: () => ({ matches: true }) },
  });
  const exports = {
    './harness.js': {
      createHarness: async (options) => { callbacks = options; return harness; },
      supported: () => true,
      WORKSPACE_ROOT: '/workspace',
    },
    './workspace.js': { WORKSPACE: {} },
    './models.js': {
      SCRIPTED_TASK: 'test the project',
      scriptedTransport: () => ({}), remoteTransport: () => ({}),
      ENDPOINTS: { fixture: { base_url: '', model: '', keyHint: '' } },
    },
  };
  const module = new vm.SourceTextModule(source, {
    context, identifier: moduleUrl.href,
    initializeImportMeta(meta) { meta.url = moduleUrl.href; },
  });
  await module.link((specifier) => {
    const values = exports[specifier];
    assert.ok(values, `unexpected import ${specifier}`);
    return new vm.SyntheticModule(Object.keys(values), function () {
      for (const [name, value] of Object.entries(values)) this.setExport(name, value);
    }, { context });
  });
  await module.evaluate();
  await module.namespace.mount(root);

  function key(key, target = document.activeElement, modifiers = {}) {
    const event = {
      key, target, ...modifiers, defaultPrevented: false, stopped: false,
      preventDefault() { this.defaultPrevented = true; },
      stopPropagation() { this.stopped = true; },
    };
    // Document capture runs before target listeners and native insertion/click.
    for (const { callback, capture } of document.listeners.get('keydown') || []) {
      assert.equal(capture, true);
      callback(event);
    }
    if (!event.stopped) target.fire('keydown', event);
    if (!event.defaultPrevented && !event.ctrlKey && !event.altKey && !event.metaKey && !event.isComposing) {
      if (target === input && key.length === 1) input.value += key;
      if (target.tag === 'button' && key === 'Enter') target.fire('click');
    }
    return event;
  }
  function start() {
    input.focus();
    input.value = 'test the project';
    key('Enter');
    assert.equal(prompts.length, 1);
  }
  const buttons = () => created.find((element) => element.className === 'choices' && element.parentNode).children;
  return { input, document, key, start, buttons, answers, steered, prompts };
}

const settled = () => new Promise((resolve) => setImmediate(resolve));

for (const [key, allowed] of [['y', true], ['n', false]]) {
  for (const focus of ['composer', 'page']) {
    test(`${key} then Enter answers permission with ${focus} focus and never queues steering`, async () => {
      const ui = await fixture();
      ui.start();
      if (focus === 'page') ui.document.body.focus();
      assert.equal(ui.key(key).defaultPrevented, true);
      ui.key('Enter');
      await settled();
      assert.deepEqual(ui.answers, [allowed]);
      assert.deepEqual(ui.steered, []);
      assert.equal(ui.input.value, '');
    });
  }
}

for (const [arrows, allowed] of [[[], true], [['ArrowDown'], false], [['ArrowDown', 'ArrowUp'], true]]) {
  test(`empty Enter confirms the highlighted choice after ${arrows.join(', ') || 'no arrows'}`, async () => {
    const ui = await fixture();
    ui.start();
    const buttons = ui.buttons();
    for (const arrow of arrows) assert.equal(ui.key(arrow).defaultPrevented, true);
    assert.equal(buttons[allowed ? 0 : 1].classList.contains('selected'), true);
    ui.input.value = '   ';
    assert.equal(ui.key('Enter').defaultPrevented, true);
    await settled();
    assert.deepEqual(ui.answers, [allowed]);
    assert.deepEqual(ui.steered, []);
  });
}

for (const [index, allowed] of [[0, true], [1, false]]) {
  test(`clicking permission row ${index} still resolves the question`, async () => {
    const ui = await fixture();
    ui.start();
    ui.buttons()[index].fire('click');
    await settled();
    assert.deepEqual(ui.answers, [allowed]);
    assert.deepEqual(ui.steered, []);
  });
}

test('Enter on a focused deny button confirms that row once', async () => {
  const ui = await fixture();
  ui.start();
  ui.buttons()[1].focus();
  ui.key('Enter');
  await settled();
  assert.deepEqual(ui.answers, [false]);
  assert.deepEqual(ui.steered, []);
});

test('nonempty drafts still steer and permission keys preserve the draft', async () => {
  const ui = await fixture();
  ui.start();
  ui.input.value = 'also check formatting';
  ui.key('Enter');
  assert.deepEqual(ui.steered, ['also check formatting']);
  assert.deepEqual(ui.answers, []);
  ui.input.value = 'keep this draft';
  ui.key('n');
  await settled();
  assert.deepEqual(ui.answers, [false]);
  assert.equal(ui.input.value, 'keep this draft');
});

test('modified and composing keys do not answer permission', async () => {
  const ui = await fixture();
  ui.start();
  for (const modifiers of [{ ctrlKey: true }, { altKey: true }, { metaKey: true }, { isComposing: true }]) {
    assert.equal(ui.key('y', ui.input, modifiers).defaultPrevented, false);
  }
  assert.deepEqual(ui.answers, []);
  ui.key('n');
  await settled();
  assert.deepEqual(ui.answers, [false]);
});

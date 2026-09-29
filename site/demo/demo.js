// The terminal on the page. It draws what the harness reports, in the shape
// the binary prints it, and turns keys into prompts, steering, cancellation,
// and answers to the permission prompt. It decides nothing about the agent:
// the turn loop, the tools, and the rules all run inside the WebAssembly
// module.

import { createHarness, supported, WORKSPACE_ROOT } from './harness.js';
import { WORKSPACE } from './workspace.js';
import { SCRIPTED_TASK, scriptedTransport, remoteTransport, ENDPOINTS } from './models.js';

const WASM_URL = new URL('./rune.wasm', import.meta.url);
const LOCAL_BASE = 'http://127.0.0.1:7777/v1';
const PERMISSION_MODE = 'ask';

export async function mount(root) {
  const screen = root.querySelector('[data-screen]');
  const input = root.querySelector('[data-input]');
  const inputRow = root.querySelector('[data-input-row]');
  const status = root.querySelector('[data-status]');
  const hint = root.querySelector('[data-hint]');
  const chip = root.querySelector('[data-model]');
  const panel = root.querySelector('[data-panel]');

  // ---------- drawing ----------

  const scrollDown = () => { screen.scrollTop = screen.scrollHeight; };

  function row(className, ...parts) {
    const line = document.createElement('div');
    line.className = 'row' + (className ? ' ' + className : '');
    for (const part of parts) {
      if (typeof part === 'string') line.append(part);
      else if (part) {
        const span = document.createElement('span');
        span.className = part[1];
        span.textContent = part[0];
        line.append(span);
      }
    }
    screen.append(line);
    scrollDown();
    return line;
  }

  const span = (text, className) => Object.assign(document.createElement('span'), { className, textContent: text });
  const notice = (text, tone = '') => row('notice ' + tone, '[' + text + ']');
  const gap = () => row('gap', ' ');

  let answer = null;
  let reasoning = null;
  let pendingTool = null;
  let context = { used: 0 };

  function textBlock(className) {
    const block = document.createElement('div');
    block.className = 'row ' + className;
    screen.append(block);
    return block;
  }

  function summarize(name, args) {
    try {
      const value = JSON.parse(args || '{}');
      return value.command || value.path || value.pattern || '';
    } catch {
      return '';
    }
  }

  // ---------- state ----------

  const state = {
    harness: null,
    running: false,
    asking: null,
    escapeArmed: 0,
    history: [],
    recall: -1,
    model: { kind: 'scripted', label: 'scripted' },
    autoplayed: false,
  };

  function drawStatus() {
    const pct = Math.min(99, Math.round((context.used / 32768) * 100));
    status.textContent = `${state.model.label} | ${PERMISSION_MODE} | ctx ${pct}% | ~/tally`;
    chip.textContent = state.model.label;
    hint.textContent = state.asking
      ? 'y allow  n deny'
      : state.running
        ? 'enter steer  esc esc cancel'
        : '/help commands  /model switch model';
  }

  function onEvent(event) {
    switch (event.kind) {
      case 'text':
        if (!answer) answer = textBlock('answer');
        answer.textContent += event.delta;
        scrollDown();
        break;
      case 'reasoning':
        if (!reasoning) reasoning = textBlock('reasoning');
        reasoning.textContent += event.delta;
        scrollDown();
        break;
      case 'tool_started':
        answer = null;
        reasoning = null;
        pendingTool = row('tool running', '  ', [event.label, 'dim'], ' ', [event.name, 'tool-name'], ' ', [summarize(event.name, event.arguments), 'faint']);
        break;
      case 'tool_output': {
        const line = pendingTool || row('tool', '');
        pendingTool = null;
        line.className = 'row tool';
        line.replaceChildren('  ', span(event.name + ': ', 'tool-name'), span(event.headline, event.is_error ? 'warn' : 'dim'));
        scrollDown();
        break;
      }
      case 'tool_denied':
        if (pendingTool) { pendingTool.remove(); pendingTool = null; }
        answer = null;
        row('tool', '  ', ['refused ' + event.name + ' ' + summarize(event.name, event.arguments), 'warn']);
        row('tool', '    ', [event.reason, 'faint']);
        break;
      case 'restarted':
        // A retry streams the answer again from the start.
        if (answer) answer.remove();
        if (reasoning) reasoning.remove();
        answer = null;
        reasoning = null;
        break;
      case 'steering':
        answer = null;
        notice(`applied ${event.count} queued message${event.count === 1 ? '' : 's'}`);
        break;
      case 'finished':
        if (event.context_tokens) context.used = event.context_tokens;
        if (event.reason === 'max_model_turns') notice('reached the model step limit', 'warn');
        break;
      case 'error':
        answer = null;
        notice(event.code === 'cancelled' ? 'cancelled' : 'the turn failed: ' + event.message, event.code === 'cancelled' ? '' : 'warn');
        if (event.hint && event.code !== 'cancelled') notice(event.hint, 'faint');
        break;
      default:
        break;
    }
    drawStatus();
  }

  // The permission prompt: the turn is suspended inside the harness until the
  // person answers, and the answer is what the rule table could not decide.
  function onAsk(question) {
    return new Promise((resolve) => {
      answer = null;
      const line = row('ask', '  allow ', [question.tool, 'tool-name'], ' ', [question.target === question.tool ? '' : question.target, 'strong'], '?  ');
      const buttons = document.createElement('span');
      buttons.className = 'choices';
      const settle = (allowed) => {
        buttons.remove();
        line.append(span(allowed ? 'allowed' : 'declined', allowed ? 'ok' : 'warn'));
        state.asking = null;
        drawStatus();
        resolve(allowed);
      };
      for (const [label, allowed] of [['y allow', true], ['n deny', false]]) {
        const button = document.createElement('button');
        button.type = 'button';
        button.textContent = label;
        button.addEventListener('click', () => settle(allowed));
        buttons.append(button);
      }
      line.append(buttons);
      state.asking = settle;
      drawStatus();
      scrollDown();
    });
  }

  // ---------- the harness ----------

  let transport = scriptedTransport();
  const route = (request) => transport(request);

  function configure() {
    const model = state.model;
    const config = model.kind === 'remote'
      ? { dialect: model.dialect, base_url: model.base_url, model: model.model, api_key: model.key || '', headers: model.headers || [], workspace: WORKSPACE_ROOT }
      : { dialect: 'chat_completions', base_url: LOCAL_BASE, model: model.model || 'scripted', workspace: WORKSPACE_ROOT };
    return state.harness.configure(config);
  }

  async function boot() {
    if (!supported()) {
      row('warn', 'This browser cannot suspend WebAssembly on a promise (JSPI), which the harness needs to wait for the network.');
      row('faint', 'Chrome and Edge 137+, Firefox 153+ and Safari 27+ have it. The install line above works everywhere.');
      input.disabled = true;
      return false;
    }
    row('faint', 'loading the harness, compiled to WebAssembly...');
    const started = performance.now();
    try {
      state.harness = await createHarness({ wasmUrl: WASM_URL, files: WORKSPACE, transport: route, onEvent, onAsk });
    } catch (err) {
      row('warn', 'the harness could not load: ' + (err && err.message ? err.message : err));
      return false;
    }
    screen.replaceChildren();
    const ms = Math.round(performance.now() - started);
    row('', ['rune', 'accent'], ' ', ['0.1.15', 'dim'], '  ', ['/help for commands', 'faint']);
    row('faint', `loaded in ${ms} ms. workspace: ~/tally, a word counter with one failing test.`);
    gap();
    configure();
    drawStatus();
    input.disabled = false;
    return true;
  }

  // ---------- prompts ----------

  async function submit(text) {
    if (!state.harness) return;
    const trimmed = text.trim();
    if (!trimmed) return;
    if (state.running) {
      const queued = state.harness.steer(trimmed);
      row('user steer', ['> ', 'accent'], trimmed);
      notice(queued ? 'queued for the next step' : 'the steering queue is full', queued ? 'faint' : 'warn');
      return;
    }
    state.history.push(trimmed);
    state.recall = -1;
    if (trimmed.startsWith('/')) {
      command(trimmed);
      return;
    }
    row('user', ['> ', 'accent'], trimmed);
    answer = null;
    reasoning = null;
    state.running = true;
    drawStatus();
    root.classList.add('busy');
    try {
      await state.harness.prompt(trimmed);
    } finally {
      state.running = false;
      state.escapeArmed = 0;
      answer = null;
      root.classList.remove('busy');
      gap();
      drawStatus();
    }
  }

  function command(line) {
    const [name, ...args] = line.split(/\s+/);
    row('user', ['> ', 'accent'], line);
    const files = state.harness.files();
    switch (name) {
      case '/help':
        for (const [cmd, what] of [
          ['/model', 'switch between the scripted model, one in this browser, or your endpoint'],
          ['/files', 'list the workspace'],
          ['/cat <path>', 'print a file'],
          ['/diff', 'show what the agent changed'],
          ['/reset', 'put the files back and start over'],
          ['/clear', 'clear the screen'],
        ]) row('', '  ', [cmd.padEnd(13), 'accent'], [what, 'dim']);
        row('faint', '  while a turn runs: enter steers it, esc twice cancels it');
        break;
      case '/files':
        for (const path of Object.keys(files).sort()) row('', '  ', [path, 'dim']);
        break;
      case '/cat': {
        const path = (args[0] || '').replace(/^\.?\//, '');
        if (!(path in files)) { notice('no such file: ' + (args[0] || ''), 'warn'); break; }
        files[path].replace(/\n$/, '').split('\n').forEach((text, index) => row('code', [String(index + 1).padStart(3) + '  ', 'faint'], text));
        break;
      }
      case '/diff': {
        const before = state.harness.original();
        let changed = 0;
        for (const path of Object.keys({ ...before, ...files }).sort()) {
          if (before[path] === files[path]) continue;
          changed++;
          row('', ['--- ' + path, 'dim']);
          const a = (before[path] || '').split('\n');
          const b = (files[path] || '').split('\n');
          const max = Math.max(a.length, b.length);
          for (let i = 0; i < max; i++) {
            if (a[i] === b[i]) continue;
            if (a[i] !== undefined) row('code', ['- ' + a[i], 'warn']);
            if (b[i] !== undefined) row('code', ['+ ' + b[i], 'ok']);
          }
        }
        if (!changed) notice('nothing has changed', 'faint');
        break;
      }
      case '/reset':
        state.harness.restore();
        state.harness.reset();
        context.used = 0;
        screen.replaceChildren();
        row('faint', 'files restored and conversation cleared.');
        gap();
        break;
      case '/clear':
        screen.replaceChildren();
        break;
      case '/model':
        openPanel();
        break;
      default:
        notice('unknown command ' + name + ', /help lists them', 'warn');
    }
    gap();
    drawStatus();
  }

  // ---------- keys ----------

  input.addEventListener('keydown', (event) => {
    if (state.asking && (event.key === 'y' || event.key === 'n')) {
      event.preventDefault();
      state.asking(event.key === 'y');
      return;
    }
    if (event.key === 'Enter') {
      event.preventDefault();
      const text = input.value;
      input.value = '';
      submit(text);
      return;
    }
    if (event.key === 'Escape') {
      event.preventDefault();
      if (!panel.hidden) { closePanel(); return; }
      if (input.value) { input.value = ''; return; }
      if (!state.running) return;
      const now = performance.now();
      if (now - state.escapeArmed < 1000) {
        state.harness.cancel();
        state.escapeArmed = 0;
        notice('cancelling the turn', 'faint');
      } else {
        state.escapeArmed = now;
        notice('press esc again to cancel', 'faint');
      }
      return;
    }
    if (event.key === 'c' && event.ctrlKey) {
      if (input.value) { input.value = ''; return; }
      if (state.running) { state.harness.cancel(); notice('cancelling the turn', 'faint'); }
      return;
    }
    if ((event.key === 'ArrowUp' || event.key === 'ArrowDown') && state.history.length) {
      event.preventDefault();
      const last = state.history.length - 1;
      if (event.key === 'ArrowUp') state.recall = state.recall < 0 ? last : Math.max(0, state.recall - 1);
      else state.recall = state.recall < 0 ? -1 : state.recall + 1 > last ? -1 : state.recall + 1;
      input.value = state.recall < 0 ? '' : state.history[state.recall];
    }
  });

  // A click anywhere in the terminal focuses the input, as in a real one,
  // unless the reader is selecting text to copy.
  root.addEventListener('mouseup', (event) => {
    if (event.target.closest('button, input, select, a, [data-panel]')) return;
    if (String(window.getSelection() || '')) return;
    input.focus({ preventScroll: true });
  });

  // ---------- the model panel ----------

  function openPanel() {
    panel.hidden = false;
    chip.setAttribute('aria-expanded', 'true');
    const first = panel.querySelector('input, select, button');
    if (first) first.focus();
  }

  function closePanel() {
    panel.hidden = true;
    chip.setAttribute('aria-expanded', 'false');
    input.focus({ preventScroll: true });
  }

  chip.addEventListener('click', () => (panel.hidden ? openPanel() : closePanel()));

  function useModel(model, nextTransport) {
    state.model = model;
    transport = nextTransport;
    state.harness.reset();
    context.used = 0;
    if (!configure()) return false;
    closePanel();
    notice('model: ' + model.label + (model.kind === 'remote' ? ' at ' + model.base_url : ''), 'faint');
    gap();
    drawStatus();
    return true;
  }

  panel.querySelector('[data-use-scripted]').addEventListener('click', () => {
    useModel({ kind: 'scripted', label: 'scripted' }, scriptedTransport());
  });

  const form = panel.querySelector('[data-endpoint]');
  const provider = form.elements.provider;
  const fill = () => {
    const preset = ENDPOINTS[provider.value];
    form.elements.base_url.value = preset.base_url;
    form.elements.model.value = preset.model;
    form.elements.key.placeholder = preset.keyHint;
  };
  provider.addEventListener('change', fill);
  fill();
  form.addEventListener('submit', (event) => {
    event.preventDefault();
    const preset = ENDPOINTS[provider.value];
    useModel({
      kind: 'remote',
      label: form.elements.model.value.trim(),
      dialect: preset.dialect,
      base_url: form.elements.base_url.value.trim(),
      model: form.elements.model.value.trim(),
      key: form.elements.key.value.trim(),
      headers: preset.headers || [],
    }, remoteTransport());
  });

  const local = panel.querySelector('[data-use-local]');
  local.addEventListener('click', async () => {
    local.disabled = true;
    const progress = panel.querySelector('[data-local-progress]');
    progress.hidden = false;
    try {
      const { loadLocalModel } = await import('./local.js');
      const engine = await loadLocalModel((fraction, text) => {
        progress.value = fraction;
        panel.querySelector('[data-local-status]').textContent = text;
      });
      useModel({ kind: 'local', label: engine.label, model: engine.id }, engine.transport);
    } catch (err) {
      panel.querySelector('[data-local-status]').textContent = err && err.message ? err.message : String(err);
      local.disabled = false;
    } finally {
      progress.hidden = true;
    }
  });

  // ---------- autoplay ----------

  // The scripted task is typed into the input as a person would type it, then
  // submitted. From that point everything on screen is the harness running.
  async function autoplay() {
    if (state.autoplayed || !state.harness) return;
    state.autoplayed = true;
    const reduce = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
    if (reduce) {
      input.value = SCRIPTED_TASK;
    } else {
      for (const char of SCRIPTED_TASK) {
        if (document.activeElement === input && input.value !== SCRIPTED_TASK.slice(0, input.value.length)) return;
        input.value += char;
        await new Promise((resolve) => setTimeout(resolve, 18 + Math.random() * 30));
      }
      await new Promise((resolve) => setTimeout(resolve, 280));
    }
    const text = input.value;
    input.value = '';
    await submit(text);
    row('faint', 'your turn: /diff shows the change, /model picks a real model, or ask for something else.');
    gap();
  }

  const ready = await boot();
  return { autoplay: ready ? autoplay : () => {}, focus: () => input.focus() };
}

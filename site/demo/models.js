// Where the harness's requests go. Each transport takes the request the
// harness built and returns a response whose body is read chunk by chunk, so
// the harness's own stream reducer sees a reply arrive the way a network
// delivers one.
//
// The scripted model answers one task with fixed replies. It is not a model:
// it is the smallest thing that lets the rest of the page run for real, with
// no download and no key. Everything it asks for is carried out by the real
// tools on the files in this tab, and every call passes the real rules.

export const SCRIPTED_TASK = 'tally counts "one  two" as three words. find the bug and fix it';

const encoder = new TextEncoder();
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function frame(delta, finish = null) {
  const chunk = {
    id: 'scripted',
    object: 'chat.completion.chunk',
    model: 'scripted',
    choices: [{ index: 0, delta, finish_reason: finish }],
  };
  return 'data: ' + JSON.stringify(chunk) + '\n\n';
}

// A reply: optional text, then optional tool calls.
function* reply({ text = '', calls = [] }) {
  if (text) {
    for (const piece of text.match(/\S+\s*/g) || []) yield [frame({ content: piece }), 22];
  }
  for (const [index, call] of calls.entries()) {
    yield [frame({
      tool_calls: [{ index, id: call.id, type: 'function', function: { name: call.name, arguments: '' } }],
    }), 60];
    const args = JSON.stringify(call.arguments);
    for (let at = 0; at < args.length; at += 12) {
      yield [frame({ tool_calls: [{ index, function: { arguments: args.slice(at, at + 12) } }] }), 14];
    }
  }
  yield [frame({}, calls.length ? 'tool_calls' : 'stop'), 0];
  yield ['data: [DONE]\n\n', 0];
}

const SCRIPT = [
  {
    text: 'I will find where words are counted.',
    calls: [{ id: 'call_1', name: 'grep_files', arguments: { pattern: 'fn count_words' } }],
  },
  {
    calls: [{ id: 'call_2', name: 'read_file', arguments: { path: 'src/count.rs' } }],
  },
  {
    text: 'Splitting on a single space turns every extra space into an empty word, and each one is counted.',
    calls: [{
      id: 'call_3',
      name: 'edit_file',
      arguments: {
        path: 'src/count.rs',
        old_string: "text.trim().split(' ').count()",
        new_string: 'text.split_whitespace().count()',
      },
    }],
  },
  {
    calls: [{ id: 'call_4', name: 'shell', arguments: { command: 'cargo test' } }],
  },
  {
    text: [
      'Fixed in src/count.rs. `count_words` now uses `split_whitespace`, so any run of',
      'spaces, tabs or newlines separates two words, and an empty input counts as zero.',
      'tests/count.rs already covers the double space. This tab cannot run cargo, so',
      'run `cargo test` on your machine to confirm.',
    ].join(' '),
  },
];

const OFF_SCRIPT = {
  text: 'This is the scripted model, and it knows one task. Type /model to use a real one, in this browser or through an endpoint you name, or /reset to watch the task again.',
};

function stream(generator) {
  const iterator = generator[Symbol.iterator]();
  let cancelled = false;
  return {
    async read() {
      if (cancelled) return { done: true };
      const next = iterator.next();
      if (next.done) return { done: true };
      const [text, pause] = next.value;
      if (pause) await sleep(pause);
      return { value: encoder.encode(text), done: false };
    },
    async cancel() {
      cancelled = true;
    },
  };
}

// Where the conversation stands: how many replies the model has given since
// the person last spoke.
function position(messages) {
  let lastUser = -1;
  messages.forEach((message, index) => {
    if (message.role === 'user') lastUser = index;
  });
  const task = lastUser >= 0 ? String(contentText(messages[lastUser].content)) : '';
  const replies = messages.slice(lastUser + 1).filter((message) => message.role === 'assistant').length;
  return { task, replies };
}

function contentText(content) {
  if (typeof content === 'string') return content;
  if (Array.isArray(content)) return content.map((part) => part.text || '').join('');
  return '';
}

export function scriptedTransport() {
  return async (request) => {
    const body = JSON.parse(request.body);
    const { task, replies } = position(body.messages || []);
    await sleep(240);
    const onScript = task.trim().toLowerCase() === SCRIPTED_TASK.toLowerCase();
    const step = onScript ? SCRIPT[replies] : replies === 0 ? OFF_SCRIPT : null;
    return {
      status: 200,
      contentType: 'text/event-stream',
      body: stream(reply(step || { text: 'Done.' })),
    };
  };
}

// A real endpoint, reached from this tab. The key goes only to the URL the
// person named, and only in memory.
export function remoteTransport() {
  return async (request) => {
    let response;
    try {
      response = await fetch(request.url, {
        method: request.method,
        headers: Object.fromEntries(request.headers),
        body: request.method === 'GET' ? undefined : request.body,
      });
    } catch (err) {
      throw new Error(
        'the endpoint could not be reached from this page. It may not accept requests from a browser (CORS), or it may be offline.',
      );
    }
    return {
      status: response.status,
      contentType: response.headers.get('content-type') || '',
      body: response.body ? response.body.getReader() : stream([][Symbol.iterator]()),
    };
  };
}

// Endpoints a visitor can pick, with what each needs.
export const ENDPOINTS = {
  openrouter: {
    label: 'OpenRouter',
    dialect: 'chat_completions',
    base_url: 'https://openrouter.ai/api/v1',
    model: 'anthropic/claude-sonnet-5',
    keyHint: 'sk-or-...',
  },
  anthropic: {
    label: 'Anthropic',
    dialect: 'anthropic',
    base_url: 'https://api.anthropic.com/v1',
    model: 'claude-sonnet-5',
    keyHint: 'sk-ant-...',
    // Anthropic serves a browser only when the page says it is one.
    headers: [['anthropic-dangerous-direct-browser-access', 'true']],
  },
  openai: {
    label: 'OpenAI',
    dialect: 'chat_completions',
    base_url: 'https://api.openai.com/v1',
    model: 'gpt-5-mini',
    keyHint: 'sk-...',
  },
  local: {
    label: 'Local server',
    dialect: 'chat_completions',
    base_url: 'http://127.0.0.1:11434/v1',
    model: 'qwen3:4b',
    keyHint: 'none needed',
  },
};

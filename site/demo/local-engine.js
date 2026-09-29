// A model running in this tab, answering the harness as an OpenAI-compatible
// endpoint would. The harness sends the request it sends to any endpoint; this
// turns it into the model's own chat template, generates, and streams the
// reply back as chat completion chunks, tool calls included. The harness's
// real stream reducer reads them, so nothing about the loop changes for a
// local model.
//
// Each family writes tool calls its own way, which is what it was trained on,
// so the adapter speaks each family's format rather than asking a small model
// for JSON it never learned to produce.

export const MODELS = {
  'qwen3.5-0.8b': {
    id: 'onnx-community/Qwen3.5-0.8B-Text-ONNX',
    label: 'Qwen3.5 0.8B',
    family: 'qwen',
    dtype: { webgpu: 'q4f16', wasm: 'q4' },
    bytes: 470e6,
    generation: { temperature: 0, top_k: 20, repetition_penalty: 1.05 },
  },
  'lfm2.5-350m': {
    id: 'onnx-community/LFM2.5-350M-ONNX',
    label: 'LFM2.5 350M',
    family: 'lfm',
    dtype: { webgpu: 'q4f16', wasm: 'q4' },
    bytes: 255e6,
    generation: { temperature: 0.1, top_k: 50, repetition_penalty: 1.05 },
  },
};

const encoder = new TextEncoder();

// ---------- request to prompt ----------

function text(content) {
  if (typeof content === 'string') return content;
  if (Array.isArray(content)) return content.map((part) => part.text || '').join('');
  return '';
}

function parseArguments(raw) {
  if (raw && typeof raw === 'object') return raw;
  try {
    return JSON.parse(raw || '{}');
  } catch {
    return {};
  }
}

const FAMILIES = {
  qwen: {
    start: '<tool_call>',
    end: '</tool_call>',
    stops: ['<|im_end|>', '<|endoftext|>'],
    // The template renders tools and past calls itself, as long as a call's
    // arguments are an object rather than the JSON text the wire carries.
    messages(request) {
      return request.messages.map((message) => {
        if (message.role === 'assistant') {
          const out = { role: 'assistant', content: text(message.content) };
          if (message.tool_calls && message.tool_calls.length) {
            out.tool_calls = message.tool_calls.map((call) => ({
              type: 'function',
              function: { name: call.function.name, arguments: parseArguments(call.function.arguments) },
            }));
          }
          return out;
        }
        return { role: message.role, content: text(message.content) };
      });
    },
    tools(request) {
      return request.tools || [];
    },
    templateOptions: { enable_thinking: false },
    parse(block, request) {
      return parseQwenCalls(block, request.tools || []);
    },
  },
  lfm: {
    start: '<|tool_call_start|>',
    end: '<|tool_call_end|>',
    stops: ['<|im_end|>', '<|endoftext|>'],
    // Tools go into the system prompt as the template expects, with the
    // instruction the model card gives for calls written as JSON, which is
    // easier to read back exactly than its default Python-like form.
    messages(request) {
      const out = [];
      for (const message of request.messages) {
        if (message.role === 'system') {
          out.push({ role: 'system', content: text(message.content) + '\nOutput function calls as JSON.' });
        } else if (message.role === 'assistant') {
          const calls = (message.tool_calls || []).map((call) => ({
            name: call.function.name,
            arguments: parseArguments(call.function.arguments),
          }));
          const written = calls.length ? this.start + JSON.stringify(calls) + this.end : '';
          out.push({ role: 'assistant', content: text(message.content) + written });
        } else if (message.role === 'tool') {
          out.push({ role: 'tool', content: text(message.content) });
        } else {
          out.push({ role: message.role, content: text(message.content) });
        }
      }
      return out;
    },
    tools(request) {
      return (request.tools || []).map((tool) => tool.function || tool);
    },
    templateOptions: {},
    parse(block) {
      return parseLfmCalls(block);
    },
  },
};

// Reads the calls a Qwen model wrote, one `<function=name>` block per call
// with a `<parameter=key>` block per argument. A value is text, so it is
// converted to the type the tool's schema declares for that argument.
export function parseQwenCalls(block, tools = []) {
  const types = new Map();
  for (const tool of tools) {
    const fn = tool.function || tool;
    types.set(fn.name, (fn.parameters && fn.parameters.properties) || {});
  }
  const calls = [];
  const functions = /<function=([^>\n]+)>([\s\S]*?)(?:<\/function>|$)/g;
  let match;
  while ((match = functions.exec(block))) {
    const name = match[1].trim();
    const schema = types.get(name) || {};
    const args = {};
    const parameters = /<parameter=([^>\n]+)>\n?([\s\S]*?)\n?<\/parameter>/g;
    let parameter;
    while ((parameter = parameters.exec(match[2]))) {
      const key = parameter[1].trim();
      args[key] = typed(parameter[2], schema[key] && schema[key].type);
    }
    calls.push({ name, arguments: args });
  }
  return calls;
}

function typed(value, type) {
  if (type === 'integer' || type === 'number') {
    const number = Number(value.trim());
    return Number.isNaN(number) ? value : number;
  }
  if (type === 'boolean') return /^true$/i.test(value.trim());
  if (type === 'object' || type === 'array') {
    try { return JSON.parse(value); } catch { return value; }
  }
  return value;
}

// Reads the calls an LFM model wrote: JSON when asked for it, and its native
// Python-like form, `[name(key="value", n=2)]`, when it falls back to that.
export function parseLfmCalls(block) {
  const body = block.trim();
  try {
    const value = JSON.parse(body);
    const list = Array.isArray(value) ? value : [value];
    return list
      .filter((call) => call && typeof call.name === 'string')
      .map((call) => ({ name: call.name, arguments: call.arguments || call.parameters || {} }));
  } catch {
    // Not JSON, so the native form.
  }
  const calls = [];
  const pattern = /([A-Za-z_][\w]*)\(((?:[^()"']|"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*')*)\)/g;
  let match;
  while ((match = pattern.exec(body))) {
    const args = {};
    const argPattern = /([A-Za-z_]\w*)\s*=\s*("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|[^,]+)/g;
    let arg;
    while ((arg = argPattern.exec(match[2]))) {
      args[arg[1]] = pythonValue(arg[2].trim());
    }
    calls.push({ name: match[1], arguments: args });
  }
  return calls;
}

function pythonValue(raw) {
  if (raw.startsWith('"')) {
    try { return JSON.parse(raw); } catch { return raw.slice(1, -1); }
  }
  if (raw.startsWith("'")) {
    const inner = raw.slice(1, -1).replace(/\\'/g, "'");
    try { return JSON.parse('"' + inner.replace(/"/g, '\\"') + '"'); } catch { return inner; }
  }
  if (raw === 'True' || raw === 'true') return true;
  if (raw === 'False' || raw === 'false') return false;
  if (raw === 'None' || raw === 'null') return null;
  const number = Number(raw);
  return Number.isNaN(number) ? raw : number;
}

// Returns the tool whose most recent call failed, if the model has already
// made that same failing call once before.
function repeatedFailure(messages) {
  const calls = [];
  for (let i = 0; i < messages.length; i++) {
    const message = messages[i];
    if (message.role !== 'assistant' || !message.tool_calls) continue;
    for (const call of message.tool_calls) {
      const result = messages.slice(i + 1).find((m) => m.role === 'tool' && m.tool_call_id === call.id);
      const failed = result && /refused|not run|cannot|No such|no tool named|error|failed/i.test(text(result.content));
      calls.push({ key: call.function.name + ' ' + call.function.arguments, name: call.function.name, failed });
    }
  }
  const last = calls[calls.length - 1];
  if (!last || !last.failed) return null;
  return calls.slice(0, -1).some((call) => call.key === last.key && call.failed) ? last.name : null;
}

// ---------- reply to stream ----------

function chunk(delta, finish = null) {
  return 'data: ' + JSON.stringify({
    id: 'local',
    object: 'chat.completion.chunk',
    model: 'local',
    choices: [{ index: 0, delta, finish_reason: finish }],
  }) + '\n\n';
}

// A body the harness reads while the model is still writing.
function channel() {
  const queue = [];
  let wake = null;
  let closed = false;
  return {
    push(textValue) {
      queue.push(encoder.encode(textValue));
      if (wake) { wake(); wake = null; }
    },
    close() {
      closed = true;
      if (wake) { wake(); wake = null; }
    },
    reader: {
      async read() {
        while (!queue.length && !closed) await new Promise((resolve) => { wake = resolve; });
        if (queue.length) return { value: queue.shift(), done: false };
        return { done: true };
      },
      async cancel() {
        closed = true;
      },
    },
  };
}

// Splits streamed text into what is shown and what belongs to a tool call,
// holding back a possible start of the marker until it is known not to be one.
function splitter(start, stops) {
  let pending = '';
  let inCall = false;
  let callText = '';
  return {
    feed(piece) {
      let shown = '';
      pending += piece;
      for (const stop of stops) pending = pending.split(stop).join('');
      while (pending) {
        if (inCall) {
          callText += pending;
          pending = '';
          break;
        }
        const at = pending.indexOf(start);
        if (at >= 0) {
          shown += pending.slice(0, at);
          pending = pending.slice(at + start.length);
          inCall = true;
          continue;
        }
        // Keep the longest suffix that could still grow into the marker.
        let keep = 0;
        for (let length = Math.min(start.length - 1, pending.length); length > 0; length--) {
          if (start.startsWith(pending.slice(-length))) { keep = length; break; }
        }
        shown += pending.slice(0, pending.length - keep);
        pending = pending.slice(pending.length - keep);
        break;
      }
      return shown;
    },
    finish() {
      const shown = inCall ? '' : pending;
      return { shown, calls: inCall ? callText : null };
    },
  };
}

export async function createLocalEngine({ transformers, key = 'lfm2.5-350m', device, onProgress = () => {} }) {
  const spec = MODELS[key];
  const family = FAMILIES[spec.family];
  const { AutoTokenizer, AutoModelForCausalLM, TextStreamer } = transformers;

  // Progress is reported across every file, weighted by size, so the bar
  // tracks the download rather than restarting for each piece.
  const files = new Map();
  const progress_callback = (event) => {
    if (event.status === 'progress' && event.total) {
      files.set(event.file, [event.loaded, event.total]);
      let loaded = 0;
      let total = 0;
      for (const [a, b] of files.values()) { loaded += a; total += b; }
      onProgress(Math.min(1, loaded / Math.max(total, spec.bytes)), `downloading ${spec.label}: ${Math.round((loaded / 1e6))} of ${Math.round(Math.max(total, spec.bytes) / 1e6)} MB`);
    } else if (event.status === 'ready' || event.status === 'done') {
      onProgress(1, `loading ${spec.label} into memory`);
    }
  };

  const tokenizer = await AutoTokenizer.from_pretrained(spec.id, { progress_callback });
  const model = await AutoModelForCausalLM.from_pretrained(spec.id, {
    dtype: spec.dtype[device] || 'q4',
    device,
    progress_callback,
  });
  onProgress(1, `${spec.label} ready`);

  let counter = 0;
  let busy = Promise.resolve();

  async function respond(request, body) {
    const messages = family.messages(request);
    // A few lines for a small model, on top of the harness's own
    // instructions: one call at a time, relative paths, and, when its last
    // call failed and it is about to repeat it, a reminder that it will fail
    // again. A larger model needs none of this.
    let note = '\nCall one tool at a time. Paths are relative to the workspace root.';
    const repeated = repeatedFailure(request.messages);
    if (repeated) note += `\nYour last call to ${repeated} failed. Do not repeat it; use what you already know.`;
    if (messages[0] && messages[0].role === 'system') {
      messages[0] = { ...messages[0], content: messages[0].content + note };
    }
    const inputs = tokenizer.apply_chat_template(messages, {
      tools: family.tools(request),
      add_generation_prompt: true,
      return_dict: true,
      ...family.templateOptions,
    });
    const split = splitter(family.start, family.stops);
    const streamer = new TextStreamer(tokenizer, {
      skip_prompt: true,
      skip_special_tokens: false,
      callback_function: (piece) => {
        const shown = split.feed(piece);
        if (shown) body.push(chunk({ content: shown }));
      },
    });
    await model.generate({
      ...inputs,
      max_new_tokens: 512,
      do_sample: spec.generation.temperature > 0,
      temperature: spec.generation.temperature,
      top_k: spec.generation.top_k,
      repetition_penalty: spec.generation.repetition_penalty,
      streamer,
    });
    const { shown, calls } = split.finish();
    if (shown) body.push(chunk({ content: shown }));
    // A family that writes one block per call may write several, so the whole
    // tail after the first marker is parsed, not only the first block.
    const written = calls === null ? [] : family.parse(spec.family === 'qwen' ? calls : calls.split(family.end)[0], request);
    // Only the first call is kept. A model this small chains guesses when it
    // writes several at once; one call at a time lets each result correct the
    // next step, which is the loop doing its job.
    const parsed = written.slice(0, 1);
    parsed.forEach((call, index) => {
      counter += 1;
      body.push(chunk({
        tool_calls: [{
          index,
          id: 'call_' + counter,
          type: 'function',
          function: { name: call.name, arguments: JSON.stringify(call.arguments) },
        }],
      }));
    });
    body.push(chunk({}, parsed.length ? 'tool_calls' : 'stop'));
    body.push('data: [DONE]\n\n');
  }

  // One generation at a time: the model has one set of weights in memory.
  const transport = async (request) => {
    const payload = JSON.parse(request.body);
    const body = channel();
    busy = busy.then(() => respond(payload, body).catch((err) => {
      body.push(chunk({ content: '\n[the local model failed: ' + (err && err.message ? err.message : err) + ']' }));
      body.push(chunk({}, 'stop'));
      body.push('data: [DONE]\n\n');
    }).finally(() => body.close()));
    return { status: 200, contentType: 'text/event-stream', body: body.reader };
  };

  return { id: spec.id, label: spec.label, device, transport };
}

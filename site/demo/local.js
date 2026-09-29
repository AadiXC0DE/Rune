// Loads a model into this tab on request. Nothing here runs until the visitor
// asks for it: the runtime and the weights are fetched then, once, and the
// browser keeps them, so a second visit reads them from disk.

import { createLocalEngine, MODELS } from './local-engine.js';

const TRANSFORMERS = 'https://cdn.jsdelivr.net/npm/@huggingface/transformers@4.3.0/+esm';

export const DEFAULT_MODEL = 'qwen3.5-0.8b';

async function gpu() {
  try {
    if (!navigator.gpu) return false;
    return Boolean(await navigator.gpu.requestAdapter());
  } catch {
    return false;
  }
}

export async function loadLocalModel(onProgress, key = DEFAULT_MODEL) {
  const spec = MODELS[key];
  const device = (await gpu()) ? 'webgpu' : 'wasm';
  onProgress(0, device === 'webgpu'
    ? `fetching the runtime for ${spec.label}`
    : `no GPU is available to this page, so ${spec.label} runs on the CPU and is slow`);
  const transformers = await import(TRANSFORMERS);
  // Weights come from the Hugging Face hub and are cached by the browser.
  transformers.env.allowLocalModels = false;
  return createLocalEngine({ transformers, key, device, onProgress });
}

# Landing page

Static files with no build step for the page itself. Serve the directory:

```sh
python3 -m http.server 8099 --directory site
```

| Path | What it is |
|---|---|
| `index.html` | The page: markup, styles, and the small scripts for the install tabs and the mark |
| `fonts/` | IBM Plex Mono, three weights, latin subset, self-hosted (SIL OFL, see `fonts/OFL.txt`) |
| `demo/rune.wasm` | The harness, compiled to WebAssembly by `cargo xtask web` |
| `demo/harness.js` | Loads the module and gives it a filesystem, a transport, a shell, and a prompt |
| `demo/demo.js` | The terminal: draws events, reads keys, and holds the model picker |
| `demo/models.js` | The scripted model and the transport for an endpoint the visitor names |
| `demo/local-engine.js` | A model running in the tab, answering as an OpenAI-compatible endpoint |
| `demo/local.js` | Loads that model on request |
| `demo/workspace.js` | The project the demo works on |
| `demo/wasi.js` | `@bjorn3/browser_wasi_shim` 0.4.2, vendored (MIT OR Apache-2.0) |

## The demo is the harness

`crates/rune-web` builds the turn loop, the file tools, the permission rules,
and the provider dialects for `wasm32-wasip1`. The page mounts an in-memory
workspace through WASI, so `read_file`, `grep_files`, `glob_files`,
`edit_file`, and `write_file` are the binary's own tools reading and writing
real (in-memory) files.

What a tab cannot provide, the page supplies through the crate's bridge:

- **Requests.** The harness builds the same request it sends from the
  terminal; the page sends it with `fetch` and hands the body back chunk by
  chunk, so the harness's own stream reducer reads it.
- **A shell.** A tab cannot start a process, so `shell` runs in a small shell
  over the same files (`ls`, `cat`, `grep`, `find`, `head`, `tail`, `wc`,
  `tree`). Anything else says it cannot run in a tab.
- **The person watching.** When no rule decides an action, the harness asks,
  and the turn is suspended until the answer arrives.

The waiting imports are wrapped with `WebAssembly.Suspending` and the prompt
export with `WebAssembly.promising` (JavaScript Promise Integration), which is
what lets blocking Rust drive an asynchronous tab. Chrome and Edge 137+,
Firefox 153+, and Safari 27+ have it; the page says so where it is missing.

Rebuild the module after changing any crate it links:

```sh
cargo xtask web
```

## Models

- **Scripted** (the default): fixed replies for one task, so the page plays
  with no download and no key. Every tool call it makes is carried out by the
  real tools and passes the real rules; a request off the script gets a reply
  saying so.
- **In this browser**: Qwen3.5 0.8B through Transformers.js, on WebGPU where
  the browser has it and on the CPU otherwise. The runtime and about 470 MB of
  weights are fetched only when the visitor presses load, and the browser keeps
  them. The adapter renders the request in the model's own chat template,
  reads its tool calls back in the format it was trained to write, and keeps
  one call per step, which a model this small needs.
- **Your endpoint**: OpenRouter, Anthropic, OpenAI, or a server on the
  visitor's machine. The key stays in the tab's memory and is sent only to the
  URL shown. An endpoint that does not answer a browser's CORS preflight
  cannot be reached from a page; the terminal says so.

## What loads when

Page load fetches the document and the fonts, about 60 KB. The harness (1.7 MB,
about 400 KB with brotli) is fetched when the demo section comes near the viewport,
and plays when it is mostly in view. Nothing else is fetched unless the
visitor picks a model that needs it.

## Editing

Colours are custom properties at the top of the stylesheet, with a light set
under `prefers-color-scheme`. The terminal stays dark in both. One accent,
square corners throughout, and motion that stops under
`prefers-reduced-motion`: the mark is drawn once and left still, and sections
appear without rising.

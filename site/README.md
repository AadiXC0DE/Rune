# Landing page

One static page: `index.html`, no build step and no dependencies. Open it
directly, or serve the directory:

```sh
python3 -m http.server 8099
```

## What it says

Every command, flag, and claim on the page was run before it was written down.
The install line matches the formula in `aadixc0de/homebrew-tap`, and the three
install paths it offers are each verified:

| Path | Verified by |
|---|---|
| `brew install aadixc0de/tap/rune` | Installing, uninstalling, and reinstalling from the pushed tap |
| clone and `cargo build --release` | The release artifacts are built this way |
| `cargo install --git ... rune` | Resolving every workspace crate from the repository |

`rune` is already the name of an unrelated language in Homebrew core, and core
wins a bare name, so the page gives the qualified tap name and says why.

## The browser demo

The demo is a second, optional layer on the same page. It is built so that a
visitor who only reads never pays for it:

- **No model and no runtime at load.** Nothing is imported, prefetched, or
  preloaded. Both runtimes are reached with a dynamic `import()` inside a click
  handler. The only requests a load makes are this file and the fonts the page
  already asked for.
- **The probe runs on click.** The built-in check is `LanguageModel.availability()`,
  called from the page and never cached, because the browser can drop its own
  model between visits.
- **The browser owns its own download.** Chrome fetches its model itself, once
  per profile, and the page wires `downloadprogress` to the `<progress>`
  element. At `e.loaded === 1` the element loses its `value` attribute so the bar
  goes indeterminate while Chrome loads the weights into memory, which is
  Chrome's own documented guidance.
- **The fallback is a real download, offered honestly.** The button labels name
  the model, the exact size, that it is one time, and that it stays on the
  device. Nothing is hidden until the built-in model has been ruled out.

### Sizes on the page

Every size is the byte count the repository publishes, rounded to the megabyte
in the label:

| Model | Bytes | Label | License |
|---|---|---|---|
| `QuantFactory/SmolLM2-135M-Instruct-GGUF` `Q4_K_M` | 105454144 | 105 MB | Apache-2.0 |
| `bartowski/Qwen2.5-0.5B-Instruct-GGUF` `Q4_K_M` | 397808192 | 398 MB | Apache-2.0 |

The page states no size for Chrome's built-in model, because Google does not
publish one and the variant differs per platform.

### Caching

wllama 3.6.1 stores weights in the Origin Private File System through its cache
manager, not in the Cache Storage API. Both the runtime and the model are served
with long-lived immutable cache headers, and the second run of the demo was
observed transferring zero model bytes, so the one-time claim on the labels holds.

### What the demo does not claim

The loop runs on a five-file sample workspace written into the page, not on your
files. The tools and the policy table in the script mirror the shape of the real
ones, including that a denied call comes back to the model as information rather
than ending the turn. The page says plainly that it demonstrates the harness,
not model quality, because a 105 MB or 398 MB model is a weak assistant and the
run will look like it.

## Editing

The colours, the type scale, and the spacing are custom properties at the top of
the stylesheet. Everything else is derived from them.

One accent colour is used across the whole page, every corner is square, and the
page is dark only. Changing any of those means changing the corresponding
property rather than a value in one section.

## Accessibility

- Every text colour passes WCAG AA against its own background.
- The install tabs are real buttons with `aria-selected` state.
- Text is readable with motion disabled; the only animation is the caret, which
  stops under `prefers-reduced-motion`.
- The layout is a single column below 860px with no horizontal overflow.

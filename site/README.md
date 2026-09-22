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

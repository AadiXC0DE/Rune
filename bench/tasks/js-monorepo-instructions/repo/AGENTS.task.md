# Store monorepo

Three packages under `packages/`: `pricing` (shared price rules), `shop` (the storefront)
and `billing` (invoices). `shop` and `billing` both call into `pricing` by relative
path, so a change in `pricing` reaches both.

Tests run per package, from the package directory:

```sh
cd packages/<name> && node --test
```

There is no root test command and no installed dependency; use only Node's standard library.
Keep every package's tests passing, not just the one you are working on.

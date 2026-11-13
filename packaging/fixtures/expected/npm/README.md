# @rune-sh/cli

Installs and runs [Rune](https://github.com/AadiXC0DE/Rune), a small native
coding agent harness.

```sh
npx @rune-sh/cli connect
npm install --global @rune-sh/cli && rune
```

On first run the launcher downloads the release archive of its own version for
this platform, checks it against the release's `SHA256SUMS`, and keeps the
binary in `~/.cache/rune-npm/<version>/` (`%LOCALAPPDATA%\rune-npm\<version>`
on Windows). Every later run starts that binary. The package has no
dependencies and no install scripts.

`RUNE_BIN` names a binary to run instead.

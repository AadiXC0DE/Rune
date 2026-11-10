# Rune for Harbor

A [Harbor](https://harborframework.com) installed-agent adapter. It installs a
static Rune binary in the task container, runs `rune ask --jsonl` with full
access and network, fills Harbor's token and cost fields from the final record
(or from the per-step records when the run was killed), and writes an ATIF
trajectory converted from the JSONL events.

`rune_harbor/parse.py` uses the standard library only and is tested offline:

```sh
python3 -m unittest discover -s bench/harbor -p 'test_*.py'
```

`rune_harbor/agent.py` imports Harbor. Its import paths follow Harbor's own fx
adapter and have not been run against an installed Harbor yet; check them on
the first run.

## Running

From the repository root, with Harbor installed and `PYTHONPATH=bench/harbor`:

```sh
harbor run -d terminal-bench-sample@2.0 -a oracle -l 2            # oracle check
harbor run -d terminal-bench-sample@2.0 -a rune_harbor.agent:Rune \
  -m deepseek/deepseek-flash -l 2 --ak binary=/path/to/rune-linux-musl
docker system prune -af                                            # after every session
```

Without `binary`, the adapter downloads the release archive for the container's
architecture (`rune-<version>-<arch>-unknown-linux-musl.tar.gz`) and checks its
`.sha256` file before installing it.

Task images are several GB each. Prune after every run; keep at least 30 GiB
free on the machine running it.

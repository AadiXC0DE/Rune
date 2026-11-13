import argparse
import sys

from .filters import LEVELS, keep_level, parse_line


def build_parser():
    parser = argparse.ArgumentParser(prog="logtool", description="Filter log files.")
    parser.add_argument("files", nargs="*", help="log files; standard input when none")
    parser.add_argument("--level", choices=LEVELS, help="keep entries at this level or above")
    return parser


def main(argv=None, stdin=None, stdout=None):
    args = build_parser().parse_args(argv)
    stdin = stdin if stdin is not None else sys.stdin
    stdout = stdout if stdout is not None else sys.stdout
    sources = []
    if args.files:
        for path in args.files:
            with open(path, encoding="utf-8") as handle:
                sources.append(handle.read().splitlines())
    else:
        sources.append(stdin.read().splitlines())
    for lines in sources:
        for line in lines:
            entry = parse_line(line)
            if entry is None:
                continue
            _, level, _ = entry
            if args.level and not keep_level(level, args.level):
                continue
            stdout.write(line + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())

import argparse
import sys

from .filters import LEVELS, keep_level, keep_since, parse_line, parse_stamp


def level_arg(text):
    value = text.upper()
    if value not in LEVELS:
        raise argparse.ArgumentTypeError(
            "invalid level %r (choose from %s)" % (text, ", ".join(LEVELS))
        )
    return value


def since_arg(text):
    try:
        return parse_stamp(text)
    except ValueError:
        raise argparse.ArgumentTypeError("invalid timestamp %r" % (text,))


def build_parser():
    parser = argparse.ArgumentParser(prog="logtool", description="Filter log files.")
    parser.add_argument("files", nargs="*", help="log files; standard input when none")
    parser.add_argument(
        "--level", type=level_arg, help="keep entries at this level or above"
    )
    parser.add_argument(
        "--since", type=since_arg, help="keep entries at or after this ISO timestamp"
    )
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
            stamp, level, _ = entry
            if args.level and not keep_level(level, args.level):
                continue
            if args.since is not None and not keep_since(stamp, args.since):
                continue
            stdout.write(line + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())

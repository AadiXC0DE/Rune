# One turn streaming 200 KiB of Markdown in 4 KiB deltas: CPU and bytes drawn.

END = "STREAM-END-MARKER"
TOTAL = 200 * 1024
DELTA = 4 * 1024


def markdown():
    lines = []
    size = 0
    number = 0
    while size < TOTAL:
        number += 1
        if number % 200 == 0:
            block = ["", "| name | value | note |", "| --- | ---: | --- |"] + [
                f"| row {row} | {row * number} | plain cell |" for row in range(5)
            ] + [""]
        elif number % 50 == 0:
            block = ["", "```rust", f"fn step_{number}() -> u64 {{", f"    {number} * 2", "}", "```", ""]
        elif number % 7 == 0:
            block = [f"- item {number} with *emphasis* and `code`", f"- another item {number}"]
        else:
            block = [f"Paragraph {number} says something ordinary about the work, with a few "
                     f"**bold** words and a [link](https://example.com/{number})."]
        lines.extend(block)
        size += sum(len(line) + 1 for line in block)
    lines.append(END)
    return "\n".join(lines) + "\n"


text = markdown()
parts = [text[start:start + DELTA] for start in range(0, len(text), DELTA)]
body = text_reply(parts)
directory, root, workspace = fresh_root("rune-perf-stream-")
server = serve(lambda request: body)
session = Session(sys.argv[1], root, server.server_port, workspace)
session.wait_for("/ commands")
session.settle()
begin = len(session.seen)
session.write(b"show me\r")
# 10 s for every 64 KiB, as the deadline for the end marker.
done = session.wait_for(END, begin, timeout=10.0 * (len(text) / 65536 + 1))
done = session.wait_for("/ commands", done, timeout=30)
terminal_bytes = done - begin
session.quit()
cpu, _ = children_usage()
print(json.dumps({"cpu_seconds": round(cpu, 3), "terminal_bytes": terminal_bytes}))

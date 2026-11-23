# One turn whose answer is 10,000 lines, then Ctrl-O: time to the viewer's first frame,
# and to the frame of a second opening (reported, not gated).

LINES = 10_000
answer = "\n".join(f"line {number} of the long answer" for number in range(1, LINES + 1))
parts = [answer[start:start + 16384] for start in range(0, len(answer), 16384)]
body = text_reply(parts)
directory, root, workspace = fresh_root("rune-perf-viewer-")
server = serve(lambda request: body)
session = Session(sys.argv[1], root, server.server_port, workspace)
session.wait_for("/ commands")
session.settle()
begin = len(session.seen)
session.write(b"long\r")
done = session.wait_for(f"line {LINES} of the long answer", begin, timeout=60)
session.wait_for("/ commands", done, timeout=30)
session.settle()
begin = len(session.seen)
started = time.monotonic()
session.write(b"\x0f")
session.wait_for("pager", begin, timeout=30)
first_frame_ms = (time.monotonic() - started) * 1000
session.write(b"q")
session.settle()
# A second opening, which renders only what changed since the first.
begin = len(session.seen)
started = time.monotonic()
session.write(b"\x0f")
session.wait_for("pager", begin, timeout=30)
reopen_ms = (time.monotonic() - started) * 1000
session.write(b"q")
session.settle()
session.quit()
print(json.dumps({"first_frame_ms": round(first_frame_ms, 2), "reopen_ms": round(reopen_ms, 2)}))

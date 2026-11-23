# One turn of 100 steps, each with ten read_file calls, then an answer: peak memory.
# Every call reads its own 2-line file, since identical results are refused as a loop.

STEPS = 100
CALLS = 10


def answer(request):
    step = sum(1 for message in request["messages"] if message["role"] == "assistant")
    if step >= STEPS:
        return text_reply(["MEMORY-DONE"])
    calls = [
        (f"call_{step}_{index}", "read_file", {"path": f"files/{step}-{index}.txt"})
        for index in range(CALLS)
    ]
    return calls_reply(calls)


directory, root, workspace = fresh_root("rune-perf-memory-")
(workspace / "files").mkdir()
for step in range(STEPS):
    for index in range(CALLS):
        (workspace / "files" / f"{step}-{index}.txt").write_text(
            f"first line of {step}-{index}\nsecond line\n"
        )
server = serve(answer)
session = Session(
    sys.argv[1], root, server.server_port, workspace,
    config="[models.chat_completions]\nid = \"perf-fixture\"\ncontext_window = 100000000\n",
)
session.wait_for("/ commands")
session.settle()
begin = len(session.seen)
session.write(b"read it many times\r")
done = session.wait_for("MEMORY-DONE", begin, timeout=300)
session.wait_for("/ commands", done, timeout=60)
session.quit()
_, rss = children_usage()
print(json.dumps({"max_rss_kib": rss}))

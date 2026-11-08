"""JSON-lines worker around the real Hermes tool functions.

Protocol: one request per line on stdin, one response per line on the original
stdout. Tool prints and import warnings go to stderr, so they cannot corrupt the
protocol.

Request:  {"id":"...", "tool":"read_file", "args":{"path":"..."}}
Response: {"id":"...", "ok":true, "elapsed_us":1234, "output":"...", "error":null}
"""

from __future__ import annotations

import json
import os
import sys
import time

HERMES = os.environ.get(
    "HERMES_HOME", r"C:\Users\Mo\AppData\Local\hermes\hermes-agent"
)
sys.path.insert(0, HERMES)

# Keep a private handle for protocol output; redirect everything else.
_real_stdout = os.fdopen(os.dup(1), "w", encoding="utf-8", buffering=1)
sys.stdout = sys.stderr

from tools.file_tools import read_file_tool, search_tool  # noqa: E402
from tools.skills_tool import skills_list  # noqa: E402
from tools.terminal_tool import terminal_tool  # noqa: E402


def call(tool: str, args: dict):
    if tool == "read_file":
        return read_file_tool(
            path=args["path"],
            offset=args.get("offset", 1),
            limit=args.get("limit", 350),
        )
    if tool == "search_files":
        return search_tool(
            pattern=args["pattern"],
            target=args.get("target", "content"),
            path=args.get("path", "."),
            limit=args.get("limit", 50),
        )
    if tool == "terminal":
        return terminal_tool(command=args["command"])
    if tool == "skills_list":
        return skills_list()
    raise ValueError(f"unsupported tool: {tool}")


def main() -> None:
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        request = json.loads(line)
        tool = request["tool"]
        args = request.get("args", {})
        started = time.perf_counter_ns()
        try:
            output = call(tool, args)
            error = None
            ok = True
        except Exception as exc:  # noqa: BLE001 - the worker must not die
            output = ""
            error = f"{type(exc).__name__}: {exc}"
            ok = False
        elapsed_us = (time.perf_counter_ns() - started) // 1000
        response = {
            "id": request.get("id"),
            "ok": ok,
            "elapsed_us": elapsed_us,
            "output": output if isinstance(output, str) else json.dumps(output),
            "error": error,
        }
        _real_stdout.write(json.dumps(response) + "\n")
        _real_stdout.flush()


if __name__ == "__main__":
    main()

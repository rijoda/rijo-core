"""Fair first pass: Rijo Core vs Hermes tools on identical operations.

One warm worker per arm, interleaved calls, 2 warmups + 30 measured reps.
The Rijo worker is `rijo --bench-stdio` (same binary, same registry); the
Hermes worker imports its real Python tool functions.

This is one session. It is evidence, not a final multi-session claim.
"""

from __future__ import annotations

import json
import os
import queue
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
BENCH = REPO / "bench"
FIXTURES = BENCH / "fixtures"
RIJO = REPO / "target" / "debug" / "rijo.exe"
HERMES_HOME = Path(
    os.environ.get("HERMES_HOME", r"C:\Users\Mo\AppData\Local\hermes\hermes-agent")
)
HERMES_PY = Path(
    os.environ.get(
        "HERMES_PY", str(HERMES_HOME / "venv" / "Scripts" / "python.exe")
    )
)
HERMES_WORKER = BENCH / "adapters" / "hermes_worker.py"
RESULT_DIR = BENCH / "results" / time.strftime("%Y-%m-%d_%H%M%S")
RAW = RESULT_DIR / "raw"
RAW.mkdir(parents=True, exist_ok=True)

WARMUP = 2
REPS = 30


class Worker:
    def __init__(self, name: str, argv: list[str], cwd: Path, env: dict[str, str]):
        self.name = name
        self.stderr_path = RESULT_DIR / f"{name}.stderr.log"
        self._stderr = open(self.stderr_path, "w", encoding="utf-8")
        self.proc = subprocess.Popen(
            argv,
            cwd=str(cwd),
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self._stderr,
            text=True,
            encoding="utf-8",
            bufsize=1,
        )
        self._lines: queue.Queue[str] = queue.Queue()
        self._reader = threading.Thread(target=self._read, daemon=True)
        self._reader.start()

    def _read(self) -> None:
        assert self.proc.stdout is not None
        for line in self.proc.stdout:
            self._lines.put(line)

    def call(self, request: dict, timeout: float = 120.0) -> dict:
        assert self.proc.stdin is not None
        self.proc.stdin.write(json.dumps(request) + "\n")
        self.proc.stdin.flush()
        try:
            line = self._lines.get(timeout=timeout)
        except queue.Empty:
            raise TimeoutError(f"{self.name} timed out on {request}")
        return json.loads(line)

    def close(self) -> None:
        self.proc.terminate()
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
        self._stderr.close()


def percentile(values: list[int], p: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    idx = max(0, min(len(ordered) - 1, round((len(ordered) - 1) * p)))
    return float(ordered[idx])


def main() -> int:
    if not RIJO.is_file():
        print(f"missing {RIJO}; run cargo build -p rijo", file=sys.stderr)
        return 2

    env = os.environ.copy()
    env["RIJO_CONFIG"] = str(BENCH / "env" / "rijo.yaml")
    env["RIJO_SKIP_TRUST"] = "1"
    env["RIJO_SKIP_ONBOARD"] = "1"
    env["PYTHONUNBUFFERED"] = "1"

    rijo = Worker("rijo", [str(RIJO), "--bench-stdio"], REPO, env)
    hermes = Worker(
        "hermes",
        [str(HERMES_PY), str(HERMES_WORKER)],
        HERMES_HOME,
        env,
    )

    cases = {
        "read_10kb": lambda i: {
            "tool": "read_file",
            "args": {"path": str(FIXTURES / "data" / f"read_{i:02d}.txt")},
            "expect": f"NEEDLE_{i:02d}",
        },
        "search_literal": lambda i: {
            "tool": "search_files",
            "args": {
                "pattern": f"needle_{i:04d}",
                "target": "content",
                "path": str(FIXTURES / "tree"),
                "limit": 5,
            },
            "expect": f"needle_{i:04d}",
        },
        "terminal_echo": lambda i: {
            "tool": "terminal",
            "args": {"command": f"echo bench_{i:02d}"},
            "expect": f"bench_{i:02d}",
        },
        "skills_list": lambda i: {
            "tool": "skills_list",
            "args": {},
            "expect": '"skills"',
        },
    }

    records: list[dict] = []
    for case, make in cases.items():
        # Two warmups with inputs that are not part of the measured series.
        for i in range(WARMUP):
            for arm in (rijo, hermes):
                req = make(i)
                arm.call({"id": f"warm-{case}-{i}", **{k: req[k] for k in ("tool", "args")}})
        for rep in range(REPS):
            i = WARMUP + rep
            order = (rijo, hermes) if rep % 2 == 0 else (hermes, rijo)
            for arm in order:
                spec = make(i)
                request = {
                    "id": f"{case}-{rep}-{arm.name}",
                    "tool": spec["tool"],
                    "args": spec["args"],
                }
                started = time.perf_counter_ns()
                response = arm.call(request)
                roundtrip_us = (time.perf_counter_ns() - started) // 1000
                output = str(response.get("output", ""))
                ok = bool(response.get("ok")) and spec["expect"] in output
                records.append(
                    {
                        "case": case,
                        "rep": rep,
                        "input": i,
                        "arm": arm.name,
                        "elapsed_us": int(response.get("elapsed_us", 0)),
                        "roundtrip_us": roundtrip_us,
                        "ok": ok,
                        "blocked": "BLOCKED" in output or "error" in output.lower()[:80],
                        "output_head": output[:160],
                        "error": response.get("error"),
                    }
                )

    rijo.close()
    hermes.close()

    raw_path = RAW / "micro_rijo_hermes.jsonl"
    with raw_path.open("w", encoding="utf-8") as fh:
        for record in records:
            fh.write(json.dumps(record, ensure_ascii=False) + "\n")

    lines = [
        "# Rijo Core vs Hermes — tool microbenchmark",
        "",
        f"Session: `{RESULT_DIR.name}` · one session, warm workers, interleaved calls.",
        f"Warmups: {WARMUP} · measured reps: {REPS} · inputs are unique per rep.",
        "",
        "| Case | Arm | n ok | median µs | p5 µs | p95 µs | vs Rijo |",
        "|---|---|---:|---:|---:|---:|---:|",
    ]
    for case in cases:
        by_arm: dict[str, list[int]] = {"rijo": [], "hermes": []}
        ok_count: dict[str, int] = {"rijo": 0, "hermes": 0}
        for record in records:
            if record["case"] != case:
                continue
            if record["ok"]:
                by_arm[record["arm"]].append(record["elapsed_us"])
                ok_count[record["arm"]] += 1
        med = {arm: statistics.median(v) if v else 0 for arm, v in by_arm.items()}
        for arm in ("rijo", "hermes"):
            vals = by_arm[arm]
            ratio = (med["hermes"] / med["rijo"]) if med["rijo"] and arm == "rijo" else 1.0
            lines.append(
                f"| {case} | {arm} | {ok_count[arm]}/{REPS} | "
                f"{med[arm]:.0f} | {percentile(vals, 0.05):.0f} | "
                f"{percentile(vals, 0.95):.0f} | {ratio:.2f}× |"
            )

    lines += [
        "",
        f"Raw: `{raw_path.relative_to(REPO)}`",
        "",
        "## What this is not",
        "",
        "- Not a claim from the old `188–1037×` series. Different inputs, warm workers,",
        "  correctness checks and unique calls per repetition.",
        "- One session, not the three-session series in the plan. Treat the medians as",
        "  a first signal, not a final factor.",
        "- Hermes blocks repeated identical calls (`BLOCKED`); the harness uses unique",
        "  paths/patterns/commands so every measured call is a real call. That guard is",
        "  itself part of the behaviour and is recorded per call.",
    ]
    report_path = RESULT_DIR / "report.md"
    report_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(report_path)
    print("\n".join(lines))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

"""Combine the three Rijo vs Hermes sessions into one honest report."""

from __future__ import annotations

import json
import statistics
from pathlib import Path

BENCH = Path(__file__).resolve().parents[1]
SESSIONS = [
    "2026-11-08_194316",
    "2026-11-08_194651",
    "2026-11-08_194930",
]
CASES = ["read_10kb", "search_literal", "terminal_echo", "skills_list"]
ARMS = ["rijo", "hermes"]
OUT = BENCH / "results" / "2026-11-08_rijo_vs_hermes.md"


def pct(values: list[int], p: float) -> int:
    ordered = sorted(values)
    idx = max(0, min(len(ordered) - 1, round((len(ordered) - 1) * p)))
    return ordered[idx]


def load(session: str) -> list[dict]:
    path = BENCH / "results" / session / "raw" / "micro_rijo_hermes.jsonl"
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


def main() -> None:
    rows = {session: load(session) for session in SESSIONS}
    lines = [
        "# Rijo Core vs Hermes — fair tool microbenchmark",
        "",
        "**Status:** first three-session signal, 2026-11-08. Warm workers, interleaved calls,",
        "unique inputs per repetition, correctness checked on every call.",
        "",
        "| | |",
        "|---|---|",
        "| Machine | same Windows 11 laptop, idle, best-performance plan |",
        "| Rijo | `rijo --bench-stdio`, commit `db698aa` — dispatch only, process warm |",
        "| Hermes | `v0.22.2+2579.g601491f`, real `tools/*` functions, Python warm |",
        "| Warmups | 2 per case/arm; measured 30 per case/arm/session |",
        "| Sessions | 3, different times; inputs are 32 distinct files/patterns/commands |",
        "| Correctness | marker/JSON check on every call |",
        "",
        "## Per-session medians (µs)",
        "",
        "| Case | Session | Rijo | Hermes | Ratio | Rijo ok | Hermes ok |",
        "|---|---|---:|---:|---:|---:|---:|",
    ]

    per_session_ratios: dict[str, list[float]] = {case: [] for case in CASES}
    for case in CASES:
        for session in SESSIONS:
            med = {}
            ok = {}
            for arm in ARMS:
                vals = [
                    r["elapsed_us"]
                    for r in rows[session]
                    if r["case"] == case and r["arm"] == arm and r["ok"]
                ]
                med[arm] = statistics.median(vals) if vals else 0
                ok[arm] = len(vals)
            ratio = med["hermes"] / med["rijo"] if med["rijo"] else 0
            per_session_ratios[case].append(ratio)
            lines.append(
                f"| {case} | {session[-6:]} | {med['rijo']:.0f} | "
                f"{med['hermes']:.0f} | {ratio:.2f}× | {ok['rijo']}/30 | {ok['hermes']}/30 |"
            )

    lines += [
        "",
        "## Pooled over all 90 repetitions",
        "",
        "| Case | Arm | n ok | median µs | p5 µs | p95 µs | Ratio range (sessions) |",
        "|---|---|---:|---:|---:|---:|---:|",
    ]
    for case in CASES:
        for arm in ARMS:
            vals = [
                r["elapsed_us"]
                for session in SESSIONS
                for r in rows[session]
                if r["case"] == case and r["arm"] == arm and r["ok"]
            ]
            lines.append(
                f"| {case} | {arm} | {len(vals)}/90 | "
                f"{statistics.median(vals):.0f} | {pct(vals, 0.05)} | "
                f"{pct(vals, 0.95)} | "
                + (
                    f"{min(per_session_ratios[case]):.2f}–{max(per_session_ratios[case]):.2f}×"
                    if arm == "rijo"
                    else "—"
                )
                + " |"
            )

    lines += [
        "",
        "## Verdict",
        "",
        "- **read_10kb — robust.** Rijo's median is 3.7–9.1 ms, Hermes' is 776–986 ms;",
        "  the distributions do not overlap in any session (Rijo p95 ≤ 30 ms, Hermes p5 ≥ 581 ms).",
        "  The factor is session-dependent (85–241×), so report the range, not one number.",
        "- **search_literal — robust in 2 of 3 sessions on p5/p95, median direction in all 3.**",
        "  Medians: 6.35–7.22×. Session 1 had outliers on both arms (Rijo max 4.7 s, Hermes max 13.3 s);",
        "  sessions 2 and 3 do not overlap at p95/p5. The honest sentence is",
        "  \"Rijo is faster by ~6–7× in the median; the search case is I/O-noisy.\"",
        "- **terminal_echo — robust.** 7.38–9.33×; Rijo's p95 stays below Hermes' p5 in all",
        "  three sessions. This is process-spawn plus shell overhead, not arithmetic.",
        "- **skills_list — indicative, not comparable.** 8.1–9.0×, but the two arms read",
        "  different skill libraries (Rijo's own + foreign, Hermes' own). The timing is",
        "  implementation-plus-library-size; do not put it in the headline until both arms",
        "  read the same fixture skills directory.",
        "",
        "## What this replaces",
        "",
        "The old `188–1037×` numbers are **not** supported by this run. They compared a warm",
        "Rijo call with single-shot Hermes calls (and in some cases a blocked repeat): the old",
        "`terminal_echo_us` of 5.9 s is close to Hermes' cold first call, not its warm median.",
        "The fair warm signal is roughly **6–9× on search/terminal** and **85–241× on a 10 KB",
        "read**, with the search caveat above.",
        "",
        "## What this is not",
        "",
        "- One machine, one OS, one session series. A different laptop or a cold page cache",
        "  changes absolute times.",
        "- OpenCode is not in this run. The plan's OpenCode adapter is still a go/no-go.",
        "- No end-to-end agent tasks, no model, no token cost. This is the tool layer only.",
        "- The `skills_list` row is not a controlled comparison (different libraries).",
        "",
        "## Raw data",
        "",
    ]
    for session in SESSIONS:
        lines.append(f"- `bench/results/{session}/raw/micro_rijo_hermes.jsonl`")
    OUT.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(OUT)
    print("\n".join(lines))


if __name__ == "__main__":
    main()

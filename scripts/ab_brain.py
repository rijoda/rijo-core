#!/usr/bin/env python3
"""A/B: What does the brain actually bring?

Arm ON:  memory.enabled = true  (index context in the prompt + brain_search tool)
Arm OFF: memory.enabled = false (neither)

Same task, same workspace, same model. Both arms share the real data_dir on
purpose — the index lives there, and an empty brain would make the comparison
meaningless. Runs are sequential, so attribution by time window is exact.

Measured per run: iterations, tool calls (by tool), read_file/search_files calls,
failures, wall time, prompt tokens, and whether the answer named the right file.

Usage: python3 scripts/ab_brain.py [--dry] [--runs N]
"""
import json, os, pathlib, shutil, subprocess, sys, time

RIJO = str(pathlib.Path(__file__).resolve().parent.parent / "rijo")
BASE_CFG = pathlib.Path.home() / ".rijo/config.yaml"
ROOT = pathlib.Path("/tmp/ab_brain")
TRACE = pathlib.Path.home() / ".rijo/data/tool_trace.jsonl"

TASK = ("Find out where this project builds its tool registry and which tools are "
        "registered there. Answer with the file path and the line numbers.")
# What a correct answer has to contain (verified by hand as well).
EXPECT = "mod.rs"


def load_trace() -> list[dict]:
    if not TRACE.exists():
        return []
    return [json.loads(l) for l in TRACE.read_text().splitlines() if l.strip()]


def make_config(tag: str, enabled: bool) -> pathlib.Path:
    lines = [
        l
        for l in BASE_CFG.read_text().splitlines()
        if not l.startswith("memory:") and not l.startswith("  enabled:")
    ]
    # The base config has a few top-level keys; append the memory block last so
    # YAML keeps a single `memory` mapping.
    lines.append("memory:")
    lines.append("  embedding_model: all-MiniLM-L6-v2")
    lines.append("  embedding_dim: 384")
    lines.append("  max_entries: 500")
    lines.append("  similarity_threshold: 0.85")
    lines.append(f"  enabled: {'true' if enabled else 'false'}")
    lines.append("  workspace_only: true")
    lines.append("  top_k: 3")
    lines.append("  min_score: 0.30")
    # The two arms must differ ONLY in the brain. The learning review would run
    # in both and spend tokens/time on top, so it is off for the measurement.
    lines.append("learning:")
    lines.append("  after_turn: false")
    cfg = ROOT / tag / "config.yaml"
    cfg.parent.mkdir(parents=True, exist_ok=True)
    cfg.write_text("\n".join(lines) + "\n")
    return cfg


def main(runs: int):
    ROOT.mkdir(parents=True, exist_ok=True)
    results = []
    for rep in range(1, runs + 1):
        for arm, enabled in (("on", True), ("off", False)):
            tag = f"{arm}{rep}"
            run_dir = ROOT / tag
            if run_dir.exists():
                shutil.rmtree(run_dir, ignore_errors=True)
            run_dir.mkdir(parents=True)
            cfg = make_config(tag, enabled)

            env = dict(os.environ)
            env.update({"RIJO_SKIP_TRUST": "1", "RIJO_SKIP_ONBOARD": "1"})
            before = len(load_trace())
            metrics_file = pathlib.Path.home() / ".rijo/data/turn_metrics.jsonl"
            before_metrics = len(metrics_file.read_text().splitlines()) if metrics_file.exists() else 0
            t0 = time.time()
            try:
                proc = subprocess.run(
                    [RIJO, "--query", TASK, "--config", str(cfg)],
                    cwd=str(os.environ.get("RIJO_AB_WORKSPACE", os.getcwd())),
                    env=env, capture_output=True, text=True, timeout=900,
                )
                out, err, rc = proc.stdout, proc.stderr, proc.returncode
            except subprocess.TimeoutExpired:
                out, err, rc = "", "TIMEOUT", -1
            wall = time.time() - t0

            calls = load_trace()[before:]
            by_tool: dict[str, int] = {}
            for c in calls:
                by_tool[c.get("tool", "?")] = by_tool.get(c.get("tool", "?"), 0) + 1
            metrics = []
            if metrics_file.exists():
                for line in metrics_file.read_text().splitlines()[before_metrics:]:
                    try:
                        d = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    if d.get("kind") == "turn":
                        metrics.append(d)
            res = {
                "arm": arm, "rep": rep, "enabled": enabled, "wall_s": round(wall, 1), "rc": rc,
                "iterations": sum(m.get("iterations", 0) for m in metrics),
                "prompt_tokens": sum(m.get("prompt_tokens", 0) for m in metrics),
                "brain_hits": sum(m.get("brain_hits", 0) for m in metrics),
                "brain_ms": sum(m.get("brain_ms", 0) for m in metrics),
                "brain_mode": sorted({m.get("brain_mode") for m in metrics}),
                "tool_calls": len(calls),
                "failures": sum(1 for c in calls if not c.get("ok", True)),
                "by_tool": by_tool,
                "answer_ok": EXPECT in out,
                "answer_tail": out.strip().splitlines()[-1][:160] if out.strip() else err[-160:],
            }
            results.append(res)
            print(json.dumps(res, ensure_ascii=False))

    (ROOT / "summary.json").write_text(json.dumps(results, indent=1, ensure_ascii=False))
    print("\n=== SUMMARY ===")
    for arm in ("on", "off"):
        rows = [r for r in results if r["arm"] == arm]
        if not rows:
            continue
        def avg(k):
            return round(sum(r[k] for r in rows) / len(rows), 1)
        print(
            f"brain {arm:3} (n={len(rows)}): iterations {avg('iterations')} | tool calls {avg('tool_calls')} "
            f"| read_file {round(sum(r['by_tool'].get('read_file',0) for r in rows)/len(rows),1)} "
            f"| search_files {round(sum(r['by_tool'].get('search_files',0) for r in rows)/len(rows),1)} "
            f"| brain_search {round(sum(r['by_tool'].get('brain_search',0) for r in rows)/len(rows),1)} "
            f"| failures {avg('failures')} | wall {avg('wall_s')}s | prompt tokens {avg('prompt_tokens')} "
            f"| correct answer {sum(1 for r in rows if r['answer_ok'])}/{len(rows)}"
        )


if __name__ == "__main__":
    args = sys.argv[1:]
    runs = 3
    if "--runs" in args:
        runs = int(args[args.index("--runs") + 1])
    if "--dry" in args:
        for rep in range(1, runs + 1):
            for arm, enabled in (("on", True), ("off", False)):
                cfg = make_config(f"{arm}{rep}", enabled)
                print(f"{arm}{rep}: enabled={enabled} cfg={cfg}")
        sys.exit(0)
    if args and "--runs" not in args:
        print("Usage: python3 scripts/ab_brain.py [--dry] [--runs N]")
        sys.exit(2)
    main(runs)

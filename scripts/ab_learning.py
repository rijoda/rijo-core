#!/usr/bin/env python3
"""A/B measurement: does a learned skill actually shorten the work?

Arm A: the skill ~/.rijo/skills/local-http-server-smoketest is present.
Arm B: the same skill is moved away, everything else stays identical.

Each run gets its own config (own data_dir -> own traces) and its own working
directory. The learning review is disabled for the duration of the measurement,
otherwise arm B would quietly recreate the skill.

Usage: python3 scripts/ab_learning.py [--dry]
       --dry only prints the configuration and starts nothing.
"""
import json, os, shutil, subprocess, sys, time, pathlib

RIJO = str(pathlib.Path(__file__).resolve().parent.parent / "rijo")
SKILL = pathlib.Path.home() / ".rijo/skills/local-http-server-smoketest"
HERMES_ROOT = pathlib.Path.home() / ".hermes/skills"
HIDDEN = pathlib.Path("/tmp/ab_hidden_skill")
ROOT = pathlib.Path("/tmp/ab")
BASE_CFG = pathlib.Path.home() / ".rijo/config.yaml"

TASK = ("The current directory contains an index.html. Start an HTTP server "
        "here on port {port}, verify with curl that it responds, and shut it "
        "down cleanly afterwards (no orphan processes).")


def load_trace() -> list[dict]:
    """Read the tool trace. It always lives in ~/.rijo/data/ because debug.rs
    ignores config.data_dir — runs are attributed by time window instead."""
    tr = pathlib.Path.home() / ".rijo/data/tool_trace.jsonl"
    if not tr.exists():
        return []
    return [json.loads(l) for l in tr.read_text().splitlines() if l.strip()]


RUNS = [("A", 1, 8801), ("B", 1, 8802), ("A", 2, 8803), ("B", 2, 8804)]


def make_config(tag: str) -> pathlib.Path:
    """Copy of the real config: own data_dir, learning review off."""
    lines = [l for l in BASE_CFG.read_text().splitlines()
             if not l.startswith("data_dir:") and not l.startswith("learning:")]
    lines.append(f"data_dir: {ROOT / tag / 'data'}")
    lines.append("learning:")
    lines.append("  after_turn: false")
    cfg = ROOT / tag / "config.yaml"
    cfg.parent.mkdir(parents=True, exist_ok=True)
    cfg.write_text("\n".join(lines) + "\n")
    return cfg


def port_free(port: int) -> bool:
    out = subprocess.run(["ss", "-ltn"], capture_output=True, text=True).stdout
    return f":{port} " not in out


def parse_run(run_dir: pathlib.Path) -> dict:
    """Metrics from the run directory, tool calls from the GLOBAL trace
    (~/.rijo/data/tool_trace.jsonl — debug.rs ignores config.data_dir).
    Attribution uses the run's time window; the runs are sequential."""
    data = run_dir / "data"
    metrics = [json.loads(l) for l in (data / "turn_metrics.jsonl").read_text().splitlines() if l.strip()] \
        if (data / "turn_metrics.jsonl").exists() else []
    turns = [m for m in metrics if m.get("kind") == "turn"]
    lo = min((m["ts"] for m in metrics), default="")
    hi = max((m["ts"] for m in metrics), default="")
    # Read the trace NOW (not once before the runs), otherwise the counters are
    # always zero.
    trace = [r for r in load_trace() if lo <= r.get("ts", "") <= hi]
    by_tool: dict[str, int] = {}
    for r in trace:
        by_tool[r.get("tool", "?")] = by_tool.get(r.get("tool", "?"), 0) + 1
    skill_calls = [r for r in trace if r.get("tool") in ("skills_list", "skill_view")]
    return {
        "iterations": sum(t.get("iterations", 0) for t in turns),
        "tool_calls_logged": sum(t.get("tool_calls", 0) for t in turns),
        "prompt_tokens": sum(t.get("prompt_tokens", 0) for t in turns),
        "completion_tokens": sum(t.get("completion_tokens", 0) for t in turns),
        "cache_hit": sum(t.get("cache_hit_tokens", 0) for t in turns),
        "cache_miss": sum(t.get("cache_miss_tokens", 0) for t in turns),
        "sys_chars": max((t.get("sys_chars", 0) for t in turns), default=0),
        "llm_ms": sum(t.get("llm_ms", 0) for t in turns),
        "tool_ms": sum(t.get("tool_ms", 0) for t in turns),
        "tool_calls": len(trace),
        "failures": sum(1 for r in trace if not r.get("ok", True)),
        "by_tool": by_tool,
        "skill_view": len(skill_calls),
        "skill_calls": [f"{r.get('tool')}: {json.dumps(r.get('args', {}), ensure_ascii=False)[:120]}"
                        for r in skill_calls][:4],
        "used_skill": any("local-http-server-smoketest" in json.dumps(r.get("args", {}))
                          for r in trace),
        "background_used": any(r.get("tool") == "terminal" and r.get("args", {}).get("background")
                               for r in trace),
        "process_manage": by_tool.get("process_manage", 0),
        "failed_calls": [{"tool": r.get("tool"), "args": json.dumps(r.get("args", {}))[:150],
                          "error": str(r.get("error"))[:120]} for r in trace if not r.get("ok", True)][:8],
    }


def main():
    if HIDDEN.exists():
        shutil.rmtree(HIDDEN, ignore_errors=True)
    ROOT.mkdir(parents=True, exist_ok=True)
    if (HERMES_ROOT / "local-http-server-smoketest").exists():
        print("WARNING: the skill also exists in the Hermes library — the A/B would be pointless")
    results = []
    print(f"Skill present before the run: {SKILL.exists()} ({SKILL})")

    for arm, rep, port in RUNS:
        tag = f"{arm}{rep}"
        run_dir = ROOT / tag
        if run_dir.exists():
            shutil.rmtree(run_dir, ignore_errors=True)
        run_dir.mkdir(parents=True)
        (run_dir / "index.html").write_text("<h1>A/B</h1>\n")
        cfg = make_config(tag)

        # Arm B: skill moved away. Arm A: skill in place.
        if arm == "B":
            if SKILL.exists():
                HIDDEN.mkdir(parents=True, exist_ok=True)
                shutil.move(str(SKILL), str(HIDDEN / SKILL.name))
        elif not SKILL.exists() and HIDDEN.exists():
            shutil.move(str(HIDDEN / SKILL.name), str(SKILL))

        if not port_free(port):
            print(f"[{tag}] port {port} is taken — skipped")
            continue

        env = dict(os.environ)
        env.update({"RIJO_SKIP_TRUST": "1", "RIJO_SKIP_ONBOARD": "1"})
        t0 = time.time()
        try:
            proc = subprocess.run(
                [RIJO, "--query", TASK.format(port=port), "--config", str(cfg)],
                cwd=str(run_dir), env=env, capture_output=True, text=True, timeout=900,
            )
            rc, out, err = proc.returncode, proc.stdout, proc.stderr
        except subprocess.TimeoutExpired:
            rc, out, err = -1, "", "TIMEOUT (900s)"
        wall = time.time() - t0

        res = parse_run(run_dir)
        res.update({"arm": arm, "rep": rep, "port": port, "wall_s": round(wall, 1), "rc": rc,
                    "skill_present": arm == "A"})
        res["answer"] = out.strip().splitlines()[-1][:200] if out.strip() else ""
        res["stderr_tail"] = err.strip().splitlines()[-1][:160] if err.strip() else ""
        leaks = not port_free(port)
        res["server_left_running"] = leaks
        if leaks:
            subprocess.run(["pkill", "-f", f"http.server {port}"], capture_output=True)
        results.append(res)
        print(json.dumps({k: v for k, v in res.items() if k not in ("failed_calls", "by_tool")},
                         ensure_ascii=False))

    # Put the skill back no matter what happened.
    if not SKILL.exists() and (HIDDEN / SKILL.name).exists():
        shutil.move(str(HIDDEN / SKILL.name), str(SKILL))
    (ROOT / "summary.json").write_text(json.dumps(results, indent=1, ensure_ascii=False))
    print("\n=== SUMMARY ===")
    for arm in ("A", "B"):
        rows = [r for r in results if r["arm"] == arm]
        if not rows:
            continue
        agg = {k: round(sum(r[k] for r in rows) / len(rows), 1)
               for k in ("iterations", "tool_calls", "failures", "wall_s", "prompt_tokens",
                         "completion_tokens", "skill_view", "cache_hit", "cache_miss",
                         "sys_chars", "process_manage")}
        print(f"Arm {arm} (skill {'on' if arm == 'A' else 'off'}, n={len(rows)}): {agg}")
    for r in results:
        print(f"  {r['arm']}{r['rep']}: iters={r['iterations']} calls={r['tool_calls']} "
              f"fail={r['failures']} wall={r['wall_s']}s skill_view={r['skill_view']} "
              f"proc_mgmt={r['process_manage']} leak={r['server_left_running']}")


if __name__ == "__main__":
    # Guard against accidental runs: this costs real model calls, so only run
    # without arguments or with --dry.
    args = [a for a in sys.argv[1:]]
    if args and args != ["--dry"]:
        print("Usage: python3 scripts/ab_learning.py [--dry]")
        print("Measurement: 2 runs per arm with/without the learned skill (costs model tokens).")
        sys.exit(2)
    if args == ["--dry"]:
        print("dry: checking configs and directories, no runs")
        ROOT.mkdir(parents=True, exist_ok=True)
        for arm, rep, port in RUNS:
            tag = f"{arm}{rep}"
            cfg = make_config(tag)
            print(f"  {tag}: port {port}, cfg {cfg}, skill_present={SKILL.exists() if arm=='A' else None}")
        sys.exit(0)
    main()

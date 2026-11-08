#!/usr/bin/env python
"""Gate E0 — evaluate an exported router bundle and calibrate per-class thresholds.

Scores the strict holdout (33 schema_tasks prompts) plus the synthetic eval
split, reports task-level recall, per-tool recall, ECE and the character
saving, then writes calibrated per-class thresholds.

Usage:
  .venv/bin/python evaluate_router.py --bundle ~/.rijo/models/router/v1
  .venv/bin/python evaluate_router.py --bundle ... --write-thresholds
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path

import numpy as np
import onnxruntime as rt
from tokenizers import Tokenizer

REPO = Path(__file__).resolve().parents[2]
DATA = REPO / "data" / "router"

# Measured per-tool schema cost in the full prompt (chars). Filled from
# --tool-chars or the registry report; used to rank thresholds by real saving.
DEFAULT_TOOL_CHARS = 500


def read_jsonl(path: Path) -> list[dict]:
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


class Scorer:
    def __init__(self, bundle: Path):
        self.tok = Tokenizer.from_file(str(bundle / "tokenizer.json"))
        self.tok.enable_truncation(max_length=256)
        self.tok.enable_padding(pad_id=1, pad_token="[PAD]")
        so = rt.SessionOptions()
        so.intra_op_num_threads = 4
        so.graph_optimization_level = rt.GraphOptimizationLevel.ORT_ENABLE_ALL
        self.sess = rt.InferenceSession(
            str(bundle / "model.onnx"), sess_options=so, providers=["CPUExecutionProvider"]
        )
        self.tools = json.loads((bundle / "labels_tools.json").read_text(encoding="utf-8"))
        self.toolsets = json.loads((bundle / "labels_toolsets.json").read_text(encoding="utf-8"))
        self.guard = json.loads((bundle / "labels_guard.json").read_text(encoding="utf-8"))
        self.compact = json.loads((bundle / "labels_compact.json").read_text(encoding="utf-8"))
        self.tool_idx = {t: i for i, t in enumerate(self.tools)}

    def score(self, texts: list[str]) -> dict[str, np.ndarray]:
        enc = self.tok.encode_batch(texts)
        ids = np.array([e.ids for e in enc], dtype=np.int64)
        mask = np.array([e.attention_mask for e in enc], dtype=np.int64)
        out = self.sess.run(["tools", "toolset", "guard", "compact"],
                            {"input_ids": ids, "attention_mask": mask})
        return {k: v for k, v in zip(("tools", "toolset", "guard", "compact"), out)}


def sigmoid(x: np.ndarray) -> np.ndarray:
    return 1.0 / (1.0 + np.exp(-x))


def ece(probs: np.ndarray, labels: np.ndarray, bins: int = 10) -> float:
    """Expected calibration error over all (row, class) pairs."""
    flat_p = probs.reshape(-1)
    flat_y = labels.reshape(-1)
    total = flat_p.size
    err = 0.0
    edges = np.linspace(0.0, 1.0, bins + 1)
    for lo, hi in zip(edges[:-1], edges[1:]):
        m = (flat_p > lo) & (flat_p <= hi)
        n = int(m.sum())
        if n == 0:
            continue
        err += (n / total) * abs(float(flat_p[m].mean()) - float(flat_y[m].mean()))
    return err


def task_metrics(scores: np.ndarray, rows: list[dict], tool_idx: dict[str, int],
                 max_tools: int, min_score: float) -> dict:
    """Task-level and per-tool recall for a threshold/cap configuration."""
    full_hits = 0
    tool_hits = 0
    tool_total = 0
    sent = 0
    per_tool_miss: dict[str, int] = {}
    per_tool_total: dict[str, int] = {}
    for i, row in enumerate(rows):
        truth = {tool_idx[t] for t in row["tools"] if t in tool_idx}
        s = scores[i].copy()
        # rank by score, keep those above the floor
        order = np.argsort(-s)
        keep = [j for j in order if s[j] >= min_score][:max_tools]
        sent += len(keep)
        if truth <= set(keep):
            full_hits += 1
        hit = len(truth & set(keep))
        tool_hits += hit
        tool_total += len(truth)
        for t in row["tools"]:
            if t not in tool_idx:
                continue
            per_tool_total[t] = per_tool_total.get(t, 0) + 1
            j = tool_idx[t]
            if j not in keep:
                per_tool_miss[t] = per_tool_miss.get(t, 0) + 1
    n = max(1, len(rows))
    return {
        "task_recall": full_hits / n,
        "tool_recall": tool_hits / max(1, tool_total),
        "avg_tools_sent": sent / n,
        "per_tool_miss": {t: per_tool_miss.get(t, 0) for t in sorted(per_tool_total)},
    }


def per_class_thresholds(scores: np.ndarray, rows: list[dict], tool_idx: dict[str, int],
                         max_tools: int, base_recall: float, grid: np.ndarray) -> tuple[dict[str, float], dict]:
    """Per-tool threshold: the highest value that keeps task recall at baseline.

    Saves characters (fewer tools above threshold) without losing a single
    full-cover task relative to the global-threshold baseline.
    """
    thresholds = {t: 0.25 for t in tool_idx}
    before = task_metrics(scores, rows, tool_idx, max_tools, 0.25)
    best = dict(before)
    for t, j in tool_idx.items():
        col = scores[:, j]
        for thr in grid:
            s = scores.copy()
            # apply a per-column floor by pushing below-threshold values to -1
            s[:, j] = np.where(col >= thr, col, -1.0)
            m = task_metrics(s, rows, tool_idx, max_tools, 0.0)
            if m["task_recall"] >= base_recall - 1e-9:
                thresholds[t] = float(thr)
                if m["avg_tools_sent"] < best["avg_tools_sent"]:
                    best = m
    return thresholds, best


def fit_temperature(logits: np.ndarray, labels: np.ndarray) -> float:
    """Single-parameter temperature scaling on the tools head.

    Fitted by grid search on the BCE — the sigmoid head is monotone, so this
    changes probabilities but never the ranking, hence never the recall.
    """
    best_t, best_nll = 1.0, float("inf")
    for t in np.arange(0.5, 6.01, 0.05):
        p = sigmoid(logits / t)
        p = np.clip(p, 1e-9, 1 - 1e-9)
        nll = -(labels * np.log(p) + (1 - labels) * np.log(1 - p)).mean()
        if nll < best_nll:
            best_nll, best_t = float(nll), float(t)
    return best_t


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bundle", default=str(Path.home() / ".rijo" / "models" / "router" / "v1"))
    ap.add_argument("--max-tools", type=int, default=8)
    ap.add_argument("--min-score", type=float, default=0.25)
    ap.add_argument("--write-thresholds", action="store_true")
    ap.add_argument("--tool-chars", type=int, default=DEFAULT_TOOL_CHARS)
    args = ap.parse_args()

    bundle = Path(args.bundle)
    scorer = Scorer(bundle)

    holdout = read_jsonl(DATA / "eval_holdout.jsonl")
    synth = read_jsonl(DATA / "eval_synth.jsonl")

    def prep(rows):
        texts = []
        for r in rows:
            prec = r.get("precursor") or []
            texts.append((" | ".join(prec) + " → " + r["text"]) if prec else r["text"])
        return texts

    def labels_for(rows, n_cols):
        labels = np.zeros((len(rows), n_cols))
        for i, row in enumerate(rows):
            for t in row["tools"]:
                if t in scorer.tool_idx:
                    labels[i, scorer.tool_idx[t]] = 1.0
        return labels

    # Temperature is fitted on the synthetic eval split and then applied to the
    # strict holdout — the holdout must stay a measurement, not a fitting set.
    temperature = 1.0
    if synth:
        synth_raw = scorer.score(prep(synth))["tools"]
        temperature = fit_temperature(synth_raw, labels_for(synth, len(scorer.tools)))
        print(f"temperature (fitted on eval_synth): {temperature:.2f}")
    report: dict = {"bundle": str(bundle), "max_tools": args.max_tools, "temperature": temperature}

    scored: dict[str, np.ndarray] = {}
    for name, rows in (("holdout", holdout), ("synth", synth)):
        if rows:
            scored[name] = sigmoid(scorer.score(prep(rows))["tools"] / temperature)

    # Operating point: temperature moves the probabilities, so the threshold has
    # to be searched *after* it. The operating point is chosen on the synthetic
    # eval split — the strict holdout stays a pure measurement and is never
    # fitted on, otherwise its recall is a training number dressed as a test.
    chosen = (args.max_tools, args.min_score)
    tune_set, tune_name = ("synth", "eval_synth") if synth else ("holdout", "holdout")
    if tune_set in scored:
        probs = scored[tune_set]
        tune_rows = synth if tune_set == "synth" else holdout
        grid = np.arange(0.05, 0.96, 0.05)
        best_op = None
        for cap in range(2, args.max_tools + 1):
            for thr in grid:
                m = task_metrics(probs, tune_rows, scorer.tool_idx, cap, float(thr))
                if m["task_recall"] >= 0.85 and (best_op is None or m["avg_tools_sent"] < best_op[2]):
                    best_op = (cap, float(thr), m["avg_tools_sent"], m["task_recall"])
        if best_op:
            chosen = (best_op[0], best_op[1])
            report["operating_point"] = {
                "max_tools": best_op[0], "min_score": best_op[1],
                "fitted_on": tune_name,
                "avg_sent": round(best_op[2], 2), "task_recall": round(best_op[3], 4),
                "note": "fewest tools sent at target task_recall 0.85",
            }
            print(f"operating point (fitted on {tune_name}): max_tools={best_op[0]} "
                  f"min_score={best_op[1]:.2f} -> {best_op[2]:.1f} tools, "
                  f"task_recall {best_op[3]:.3f}")
        else:
            # No operating point clears the target on the tuning split — the
            # recall ceiling of the tuning split bounds what the holdout can do.
            ceiling, ceiling_recall = None, -1.0
            for cap in range(2, args.max_tools + 1):
                m = task_metrics(probs, tune_rows, scorer.tool_idx, cap, 0.0)
                if m["task_recall"] > ceiling_recall:
                    ceiling, ceiling_recall = m, m["task_recall"]
            report["operating_point"] = {
                "max_tools": args.max_tools, "min_score": 0.0, "fitted_on": tune_name,
                "note": "target not reachable; sending the full list",
            }
            report["recall_ceiling"] = {"split": tune_name, "task_recall": round(ceiling_recall, 4)}
            print(f"{tune_name} recall ceiling {ceiling_recall:.3f} (all tools, no threshold) "
                  f"— below the 0.85 target")
            chosen = (args.max_tools, 0.0)
    args.max_tools, args.min_score = chosen
    report["max_tools"] = chosen[0]
    report["min_score"] = chosen[1]

    for name, rows in (("holdout", holdout), ("synth", synth)):
        if not rows:
            continue
        probs = scored[name]
        labels = labels_for(rows, len(scorer.tools))
        m = task_metrics(probs, rows, scorer.tool_idx, args.max_tools, args.min_score)
        block = {
            "rows": len(rows),
            "max_tools": args.max_tools,
            "min_score": args.min_score,
            "task_recall": round(m["task_recall"], 4),
            "tool_recall": round(m["tool_recall"], 4),
            "avg_tools_sent": round(m["avg_tools_sent"], 2),
            "ece": round(ece(probs, labels), 4),
            # Fraction of the 29 tools not sent — a tool-count proxy. The real
            # character saving is measured in Rust (schema_eval sent_chars_per_run,
            # decision_trace tools_chars_sent) because the schemas differ in size.
            "tool_reduction": round(1.0 - m["avg_tools_sent"] / len(scorer.tools), 4),
            "worst_tools": sorted(m["per_tool_miss"].items(), key=lambda kv: -kv[1])[:6],
        }
        report[name] = block
        print(f"[{name}] rows={block['rows']} task_recall={block['task_recall']:.3f} "
              f"tool_recall={block['tool_recall']:.3f} avg_tools={block['avg_tools_sent']:.1f} "
              f"ECE={block['ece']:.3f} tool_reduction={block['tool_reduction']*100:.0f}%")

    # Gate E0
    h = report.get("holdout")
    if h:
        ok = h["task_recall"] >= 0.85 and h["ece"] < 0.10
        report["gate_E0"] = {
            "task_recall>=0.85": h["task_recall"] >= 0.85,
            "ece<0.10": h["ece"] < 0.10,
            "pass": bool(ok),
        }
        print("E0:", "PASS" if ok else "FAIL", report["gate_E0"])

    if args.write_thresholds and h:
        # Per-class thresholds are fitted on the *training* validation split.
        # Fitting them on the synthetic eval split looked fine there (8.00 →
        # 2.41 tools at unchanged recall) but collapsed on the holdout
        # (0.909 → 0.030): the templated families are not the same difficulty as
        # real prompts, so their score distribution does not transfer.
        val_rows = read_jsonl(DATA / "val.jsonl") if (DATA / "val.jsonl").exists() else []
        if val_rows:
            val_scores = sigmoid(scorer.score(prep(val_rows))["tools"] / temperature)
            base = task_metrics(val_scores, val_rows, scorer.tool_idx,
                                cap := args.max_tools, args.min_score)
            thresholds, best = per_class_thresholds(
                val_scores, val_rows, scorer.tool_idx, cap, base["task_recall"], grid
            )
            # Reject the calibration if it does not hold on its own fitting
            # split: a threshold that needs the last point of recall is a
            # threshold that will miss on the next prompt distribution.
            if best["task_recall"] < base["task_recall"] - 1e-9:
                thresholds = {t: args.min_score for t in scorer.tool_idx}
                best = base
                note = "rejected: recall dropped on the fitting split"
            else:
                note = "accepted"
            # The holdout is reported, never used to choose.
            hold_base = task_metrics(scored["holdout"], holdout, scorer.tool_idx,
                                     cap, args.min_score)
            calibrated_scores = scored["holdout"].copy()
            for name, thr in thresholds.items():
                j = scorer.tool_idx.get(name)
                if j is not None:
                    col = calibrated_scores[:, j]
                    calibrated_scores[:, j] = np.where(col >= thr, col, -1.0)
            hold_after = task_metrics(calibrated_scores, holdout, scorer.tool_idx, cap, 0.0)
            report["calibrated"] = {
                "max_tools": cap,
                "temperature": temperature,
                "fitted_on": "val",
                "status": note,
                "val_base": {k: round(v, 4) for k, v in base.items() if k != "per_tool_miss"},
                "val_after": {k: round(v, 4) for k, v in best.items() if k != "per_tool_miss"},
                "holdout_with_global_threshold": {
                    k: round(v, 4) for k, v in hold_base.items() if k != "per_tool_miss"},
                "holdout_with_per_tool_threshold": {
                    k: round(v, 4) for k, v in hold_after.items() if k != "per_tool_miss"},
                "thresholds": thresholds,
            }
            print(f"per-class thresholds ({note}): val tools {base['avg_tools_sent']:.2f} "
                  f"-> {best['avg_tools_sent']:.2f} at recall {base['task_recall']:.3f}")
            print(f"holdout (reported, not fitted): task_recall "
                  f"{hold_base['task_recall']:.3f} -> {hold_after['task_recall']:.3f}, "
                  f"tools {hold_base['avg_tools_sent']:.1f} -> {hold_after['avg_tools_sent']:.1f}")
        else:
            thresholds = {t: args.min_score for t in scorer.tool_idx}
        (bundle / "thresholds.json").write_text(json.dumps(
            {"per_tool": thresholds, "max_tools": args.max_tools, "min_score": args.min_score,
             "temperature": temperature},
            indent=2))
        # config.json must agree with thresholds.json — RouterSession prefers the
        # calibrated file, and two disagreeing operating points is a trap.
        cfg_path = bundle / "config.json"
        if cfg_path.exists():
            cfg = json.loads(cfg_path.read_text(encoding="utf-8"))
            cfg["min_score"] = args.min_score
            cfg["max_tools"] = args.max_tools
            cfg["temperature"] = temperature
            cfg_path.write_text(json.dumps(cfg, indent=2), encoding="utf-8")
        print(f"wrote {bundle / 'thresholds.json'}" + (" + config.json" if cfg_path.exists() else ""))

    out = bundle / "eval_report.json"
    out.write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(f"report -> {out}")


if __name__ == "__main__":
    main()

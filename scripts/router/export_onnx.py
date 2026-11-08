#!/usr/bin/env python
"""M4 — export the trained router heads into ONE ONNX graph + sidecar metadata.

Input : runs/router/v1/seed<seed>/  (HF save from train.py)
Output: ~/.rijo/models/router/v1/   (consumed by RouterSession in M3)
         model.onnx, tokenizer.json, labels_tools.json, labels_toolsets.json,
         labels_guard.json, labels_compact.json, thresholds.json,
         config.json, card.md

The ONNX graph takes (input_ids[int64 2D], attention_mask[int64 2D]) and
returns four float32 tensors: tools, toolset, guard, compact.
"""

from __future__ import annotations

import argparse
import json
import shutil
from pathlib import Path

import numpy as np
import torch
from transformers import AutoModel, AutoTokenizer

REPO = Path(__file__).resolve().parents[2]
DEFAULT_OUT = Path.home() / ".rijo" / "models" / "router" / "v1"
GUARD_LABELS = [
    "approved-env",
    "approved-by-user",
    "denied-headless",
    "denied-by-user",
    "self-approval-rejected",
]
COMPACT_LABELS = ["drop", "keep", "stub"]


class RouterGraph(torch.nn.Module):
    def __init__(self, model_dir: Path, n_tools: int, n_toolsets: int, n_guard: int, n_compact: int):
        super().__init__()
        enc = AutoModel.from_pretrained(model_dir)
        h = getattr(enc.config, "dim", None) or enc.config.hidden_size
        self.enc = enc
        self.tools = torch.nn.Linear(h, n_tools)
        self.toolset = torch.nn.Linear(h, n_toolsets)
        self.guard = torch.nn.Linear(h, n_guard)
        self.compact = torch.nn.Linear(h, n_compact)

    def forward(self, input_ids: torch.Tensor, attention_mask: torch.Tensor):
        cls = self.enc(input_ids=input_ids, attention_mask=attention_mask).last_hidden_state[:, 0]
        return self.tools(cls), self.toolset(cls), self.guard(cls), self.compact(cls)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--model-dir", default=str(REPO / "runs" / "router" / "v1" / "seed42"))
    ap.add_argument("--out", default=str(DEFAULT_OUT))
    ap.add_argument("--opset", type=int, default=17)
    ap.add_argument("--min-score", type=float, default=0.25)
    ap.add_argument("--max-tools", type=int, default=8)
    ap.add_argument("--model-name", default="distilbert-base-multilingual-cased")
    args = ap.parse_args()

    model_dir = Path(args.model_dir)
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)

    labels = json.loads((Path.home() / ".rijo" / "data" / "router" / "labels_registry.json").read_text(encoding="utf-8"))
    tools = [t["name"] for t in labels["tools"]]
    toolsets = sorted({t["toolset"] for t in labels["tools"]})

    # Head weights: train.py only saved the encoder + tokenizer. Heads are
    # randomly initialised per seed, so we must retrain heads or re-run.
    heads_dir = model_dir / "heads.pt"
    n_guard, n_compact = len(GUARD_LABELS), len(COMPACT_LABELS)
    model = RouterGraph(model_dir, len(tools), len(toolsets), n_guard, n_compact)
    if heads_dir.exists():
        sd = torch.load(heads_dir, map_location="cpu")
        model.tools.load_state_dict(sd["tools"])
        model.toolset.load_state_dict(sd["toolset"])
        model.guard.load_state_dict(sd["guard"])
        model.compact.load_state_dict(sd["compact"])
    else:
        raise SystemExit(f"missing {heads_dir} — train.py must save heads")

    model.eval()
    enc = torch.onnx.export(
        model,
        (torch.ones(1, 16, dtype=torch.long), torch.ones(1, 16, dtype=torch.long)),
        str(out / "model.onnx"),
        input_names=["input_ids", "attention_mask"],
        output_names=["tools", "toolset", "guard", "compact"],
        dynamic_axes={
            "input_ids": {0: "batch", 1: "seq"},
            "attention_mask": {0: "batch", 1: "seq"},
            "tools": {0: "batch"},
            "toolset": {0: "batch"},
            "guard": {0: "batch"},
            "compact": {0: "batch"},
        },
        opset_version=args.opset,
        do_constant_folding=True,
    )

    tok = AutoTokenizer.from_pretrained(model_dir)
    tok.save_pretrained(out)

    (out / "labels_tools.json").write_text(json.dumps(tools, indent=2), encoding="utf-8")
    (out / "labels_toolsets.json").write_text(json.dumps(toolsets, indent=2), encoding="utf-8")
    (out / "labels_guard.json").write_text(json.dumps(GUARD_LABELS, indent=2), encoding="utf-8")
    (out / "labels_compact.json").write_text(json.dumps(COMPACT_LABELS, indent=2), encoding="utf-8")
    (out / "thresholds.json").write_text(json.dumps({"tools": args.min_score, "max_tools": args.max_tools}, indent=2), encoding="utf-8")
    (out / "config.json").write_text(json.dumps({
        "version": "v1",
        "model": args.model_name,
        "max_len": 256,
        # guard stays off: guard_trace.jsonl holds 388 rows but only 32 distinct
        # texts (three classes have a single training example each). Gate E0b
        # needs a data collection phase first — see docs/GOAL-100.md.
        "heads": {"tools": True, "toolset": True, "guard": False, "compact": True},
        "min_score": args.min_score,
        "max_tools": args.max_tools,
        "opset": args.opset,
    }, indent=2))
    print(f"exported -> {out}")


if __name__ == "__main__":
    main()

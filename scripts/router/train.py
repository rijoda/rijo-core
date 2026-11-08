#!/usr/bin/env python
"""M2 — fine-tune the DistilBERT multi-head router on the exported dataset.

Four heads share one encoder:
  H1 tools    — 29-way multi-label (asymmetric loss)
  H2 toolset  — 8-way single-label (CE)
  H3 guard    — 5-way single-label (CE)
  H4 compact  — 3-way single-label per message (CE: keep / stub / drop)

Only rows whose `source`/head matches contribute to a head, so guard/compact
rows (which have a `label` instead of `tools`) train their own head only.

Device: `--device auto` (default) uses CUDA when visible, else CPU. Single
process on every device. On CPU: 4 threads and 3 frozen encoder layers, which is
what this script always did. On GPU: all layers train and AMP is enabled — see
`--freeze-layers` / `--amp` for overrides.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import random
import time
from dataclasses import dataclass
from pathlib import Path

import numpy as np
import torch
import torch.nn as nn
import torch.nn.functional as F
from torch.utils.data import DataLoader, Dataset
from transformers import AutoModel, AutoTokenizer

# AMP moved into torch.amp in 2.4+; torch.cuda.amp still works but warns.
try:
    from torch.amp import GradScaler as _GradScaler, autocast as _autocast
    _NEW_AMP = True
except ImportError:  # torch < 2.4
    from torch.cuda.amp import GradScaler as _GradScaler, autocast as _autocast
    _NEW_AMP = False


class AsymmetricLoss(nn.Module):
    """ASL (Ridnik et al.) — pinpoints rare positives, suppresses easy negatives.

    Implemented here instead of importing: transformers dropped the public
    `AsymmetricLoss` symbol in 4.4x.
    """

    def __init__(self, gamma_neg: float = 4.0, gamma_pos: float = 1.0, clip: float = 0.05):
        super().__init__()
        self.gamma_neg, self.gamma_pos, self.clip = gamma_neg, gamma_pos, clip

    def forward(self, logits: torch.Tensor, targets: torch.Tensor) -> torch.Tensor:
        xs_pos = torch.sigmoid(logits)
        xs_neg = 1.0 - xs_pos
        if self.clip > 0:
            xs_neg = (xs_neg + self.clip).clamp(max=1.0)
        loss_pos = targets * torch.log(xs_pos.clamp(min=1e-8))
        loss_neg = (1 - targets) * torch.log(xs_neg.clamp(min=1e-8))
        loss = loss_pos + loss_neg
        with torch.no_grad():
            pt = xs_pos * targets + xs_neg * (1 - targets)
            gamma = self.gamma_pos * targets + self.gamma_neg * (1 - targets)
            w = torch.pow(1 - pt, gamma)
        return -(loss * w).sum(dim=1).mean()

REPO = Path(__file__).resolve().parents[2]
DATA = REPO / "data" / "router"
LABELS = Path(os.environ.get("HOME", "~")) / ".rijo" / "data" / "router" / "labels_registry.json"
BASE_MODEL = "distilbert-base-multilingual-cased"


def pick_device(requested: str) -> torch.device:
    """Resolve --device. Never silently fall back to CPU when CUDA was asked for."""
    if requested == "cpu":
        return torch.device("cpu")
    have = torch.cuda.is_available()
    if requested == "cuda" and not have:
        raise SystemExit(
            "--device cuda was requested but no CUDA device is visible to torch.\n"
            "On a GPU host install the CUDA build, not the CPU wheel:\n"
            "  pip install torch --index-url https://download.pytorch.org/whl/cu124\n"
            "and check `python -c \"import torch; print(torch.__version__, torch.cuda.is_available())\"`.")
    if requested == "auto":
        return torch.device("cuda" if have else "cpu")
    return torch.device(requested)


def resolve_amp(device: torch.device, requested: str):
    """Return (enabled, dtype). fp16 needs a GradScaler, bf16 does not."""
    if device.type != "cuda" or requested == "off":
        return False, None
    if requested in ("bf16", "auto") and torch.cuda.is_bf16_supported():
        return True, torch.bfloat16
    if requested in ("fp16", "auto"):
        return True, torch.float16
    return False, None


def make_scaler(enabled: bool, dtype):
    # GradScaler is only meaningful for fp16; bf16 keeps full range already.
    on = bool(enabled and dtype is torch.float16)
    if _NEW_AMP:
        return _GradScaler("cuda", enabled=on)
    return _GradScaler(enabled=on)


def amp_ctx(device: torch.device, enabled: bool, dtype):
    if not enabled:
        return torch.autocast(device_type=device.type, enabled=False)
    if _NEW_AMP:
        return _autocast(device_type=device.type, dtype=dtype)
    return _autocast(enabled=True)


GUARD_LABELS = [
    "approved-env",
    "approved-by-user",
    "denied-headless",
    "denied-by-user",
    "self-approval-rejected",
]
COMPACT_LABELS = ["drop", "keep", "stub"]


def read_jsonl(path: Path) -> list[dict]:
    with path.open(encoding="utf-8") as fh:
        return [json.loads(line) for line in fh if line.strip()]


def load_labels(path: Path) -> tuple[list[str], list[str]]:
    if not path.is_file():
        raise SystemExit(
            f"labels registry missing: {path}\n"
            f"It is written by the Rust CLI (rijo --dump-labels) and defines the\n"
            f"EXACT neuron order of the tool labels. Regenerate it rather than\n"
            f"hand-writing it -- a different order permutes the trained heads\n"
            f"silently. Pass --labels <path> to point at a copy.")
    reg = json.loads(path.read_text(encoding="utf-8"))
    tools = [t["name"] for t in reg["tools"]]
    toolsets = sorted({t["toolset"] for t in reg["tools"]})
    return tools, toolsets


@dataclass
class Example:
    text: str
    tools: list[int]
    toolset: int
    guard: int
    compact: int


def build_examples(
    rows: list[dict],
    tool_idx: dict[str, int],
    toolset_idx: dict[str, int],
    head: str,
) -> list[Example]:
    out: list[Example] = []
    for row in rows:
        text = row["text"]
        prec = row.get("precursor") or []
        if prec:
            text = " | ".join(prec) + " → " + text
        tools = [tool_idx[t] for t in row.get("tools", []) if t in tool_idx]
        toolsets = [toolset_idx[t] for t in row.get("toolsets", []) if t in toolset_idx]
        label = row.get("label")
        if head == "tools":
            ex = Example(text, tools, toolsets[0] if toolsets else 0, -1, -1)
        elif head == "guard":
            ex = Example(text, [], 0, GUARD_LABELS.index(label), -1)
        else:
            ex = Example(text, [], 0, -1, COMPACT_LABELS.index(label))
        out.append(ex)
    return out


class HeadDataset(Dataset):
    def __init__(self, examples: list[Example], enc: dict[str, torch.Tensor], max_len: int):
        self.ex = examples
        self.ids = enc["input_ids"]
        self.att = enc["attention_mask"]

    def __len__(self) -> int:
        return len(self.ex)

    def __getitem__(self, i: int):
        e = self.ex[i]
        n = len(self.ids[i])
        return {
            "input_ids": torch.tensor(self.ids[i], dtype=torch.long),
            "attention_mask": torch.tensor(self.att[i], dtype=torch.long),
            "tools": torch.tensor(e.tools, dtype=torch.long),
            "toolset": e.toolset,
            "guard": e.guard,
            "compact": e.compact,
        }


def collate(batch: list[dict], pad_id: int) -> dict:
    n = max(len(b["input_ids"]) for b in batch)
    n = min(n, 256)
    ids = torch.full((len(batch), n), pad_id, dtype=torch.long)
    att = torch.zeros((len(batch), n), dtype=torch.long)
    for i, b in enumerate(batch):
        k = len(b["input_ids"])
        ids[i, :k] = b["input_ids"][:n]
        att[i, :k] = b["attention_mask"][:n]
    return {
        "input_ids": ids,
        "attention_mask": att,
        "tools": [b["tools"] for b in batch],
        "toolset": torch.tensor([b["toolset"] for b in batch]),
        "guard": torch.tensor([b["guard"] for b in batch]),
        "compact": torch.tensor([b["compact"] for b in batch]),
    }


class Router(nn.Module):
    def __init__(self, model_name: str, n_tools: int, n_toolsets: int, n_guard: int, n_compact: int,
                 dropout: float = 0.1):
        super().__init__()
        self.enc = AutoModel.from_pretrained(model_name)
        # DistilBERT exposes `dim`; XLM-R/RoBERTa/BERT expose `hidden_size`.
        h = getattr(self.enc.config, "dim", None) or self.enc.config.hidden_size
        self.tools = nn.Linear(h, n_tools)
        self.toolset = nn.Linear(h, n_toolsets)
        self.guard = nn.Linear(h, n_guard)
        self.compact = nn.Linear(h, n_compact)
        self.dropout = nn.Dropout(dropout)

    def forward(self, input_ids, attention_mask):
        cls = self.enc(input_ids=input_ids, attention_mask=attention_mask).last_hidden_state[:, 0]
        cls = self.dropout(cls)
        return self.tools(cls), self.toolset(cls), self.guard(cls), self.compact(cls)


def multi_label_targets(batch: list[torch.Tensor], n_tools: int, device) -> torch.Tensor:
    y = torch.zeros(len(batch), n_tools, device=device)
    for i, idx in enumerate(batch):
        if len(idx):
            y[i, idx] = 1.0
    return y


def mapk(scores: torch.Tensor, targets: list[torch.Tensor], k: int) -> float:
    """Mean average precision @ k for multi-label targets (per-row AP@top-k)."""
    k = min(k, scores.shape[1])
    order = scores.topk(k, dim=1).indices
    aps = []
    for i, idx in enumerate(targets):
        if len(idx) == 0:
            continue
        ranked = order[i].tolist()
        hits = 0
        ap = 0.0
        for rank, label in enumerate(ranked, start=1):
            if label in set(idx.tolist()):
                hits += 1
                ap += hits / rank
        ap /= len(idx)
        aps.append(ap)
    return float(np.mean(aps)) if aps else 0.0


def recall_at_k(scores: torch.Tensor, targets: list[torch.Tensor], k: int) -> float:
    k = min(k, scores.shape[1])
    order = scores.topk(k, dim=1).indices
    hits = [len(set(order[i].tolist()) & set(idx.tolist())) / len(idx) for i, idx in enumerate(targets) if len(idx)]
    return float(np.mean(hits)) if hits else 0.0


def accuracy(logits: torch.Tensor, labels: torch.Tensor) -> float:
    return float((logits.argmax(-1) == labels).float().mean())


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--seeds", type=int, nargs="+", default=[42])
    ap.add_argument("--epochs", type=int, default=3)
    ap.add_argument("--batch-size", type=int, default=16)
    ap.add_argument("--lr", type=float, default=3e-5)
    ap.add_argument("--head-lr", type=float, default=1e-3)
    ap.add_argument("--model", default=BASE_MODEL)
    ap.add_argument("--out", default=str(REPO / "runs" / "router" / "v1"))
    ap.add_argument("--max-train", type=int, default=0, help="debug: cap rows per head")
    ap.add_argument("--freeze-layers", type=int, default=-1,
                    help="freeze embeddings + bottom N encoder layers. "
                         "-1 (default) = 3 on CPU (legacy speedup), 0 on GPU (full fine-tune)")
    ap.add_argument("--device", default="auto", choices=["auto", "cpu", "cuda"],
                    help="auto uses CUDA when visible. 'cuda' fails loudly instead of "
                         "silently training on CPU and billing you for a GPU.")
    ap.add_argument("--amp", default="auto", choices=["auto", "off", "bf16", "fp16"],
                    help="mixed precision on CUDA; ignored on CPU")
    ap.add_argument("--labels", default=None,
                    help=f"path to labels_registry.json (default {LABELS})")
    ap.add_argument("--threads", type=int, default=0,
                    help="torch CPU threads; 0 = auto (4 on CPU, no change on GPU)")
    ap.add_argument("--dropout", type=float, default=0.1,
                    help="classifier dropout; the head overfits early on this small set")
    ap.add_argument("--label-smoothing", type=float, default=0.0,
                    help="label smoothing for the toolset/guard/compact CE heads")
    ap.add_argument("--wd", type=float, default=0.01, help="AdamW weight decay")
    ap.add_argument("--sched", default="none", choices=["none", "cosine"],
                    help="LR schedule; 'none' (constant LR) measured best on this set")
    ap.add_argument("--heads", default="tools,toolset,guard,compact",
                    help="comma-separated heads to train (guard is data-starved: 23 train rows)")
    args = ap.parse_args()
    active_heads = {h.strip() for h in args.heads.split(",") if h.strip()}

    device = pick_device(args.device)
    amp_on, amp_dtype = resolve_amp(device, args.amp)
    if args.freeze_layers < 0:
        args.freeze_layers = 3 if device.type == "cpu" else 0
    if args.threads > 0 or device.type == "cpu":
        torch.set_num_threads(args.threads if args.threads > 0 else 4)
    if device.type == "cuda":
        torch.backends.cuda.matmul.allow_tf32 = True
        torch.backends.cudnn.allow_tf32 = True

    # Printed first, on purpose: if this line does not say cuda, a GPU rental
    # is burning money on CPU work.
    print(f"device={device.type} amp={'off' if not amp_on else amp_dtype} "
          f"freeze_layers={args.freeze_layers} threads={torch.get_num_threads()} "
          f"batch={args.batch_size} lr={args.lr} heads={args.head_lr}")
    if device.type == "cuda":
        print(f"gpu={torch.cuda.get_device_name(0)} "
              f"vram={torch.cuda.get_device_properties(0).total_memory / 2**30:.1f}GiB")

    label_path = Path(args.labels).expanduser() if args.labels else LABELS
    tools, toolsets = load_labels(label_path)
    tool_idx = {t: i for i, t in enumerate(tools)}
    toolset_idx = {t: i for i, t in enumerate(toolsets)}
    print(f"labels={label_path} tools={len(tools)} toolsets={len(toolsets)} "
          f"({', '.join(toolsets)})")

    tok = AutoTokenizer.from_pretrained(args.model)

    def tokenize(rows: list[dict]) -> list[Example]:
        return rows

    tr_rows = read_jsonl(DATA / "train.jsonl")
    va_rows = read_jsonl(DATA / "val.jsonl")
    guard_rows = [r for r in read_jsonl(DATA / "guard.jsonl") if r["split"] == "train"]
    guard_va = [r for r in read_jsonl(DATA / "guard.jsonl") if r["split"] == "val"]
    comp_rows = [r for r in read_jsonl(DATA / "compact.jsonl") if r["split"] == "train"]
    comp_va = [r for r in read_jsonl(DATA / "compact.jsonl") if r["split"] == "val"]

    heads = {
        "tools": (build_examples(tr_rows, tool_idx, toolset_idx, "tools"),
                  build_examples(va_rows, tool_idx, toolset_idx, "tools")),
        "guard": (build_examples(guard_rows, tool_idx, toolset_idx, "guard"),
                  build_examples(guard_va, tool_idx, toolset_idx, "guard")),
        "compact": (build_examples(comp_rows, tool_idx, toolset_idx, "compact"),
                    build_examples(comp_va, tool_idx, toolset_idx, "compact")),
    }
    for name, (tr, va) in heads.items():
        if args.max_train:
            tr, va = tr[: args.max_train], va[: max(16, args.max_train // 4)]
            heads[name] = (tr, va)
        print(f"head {name}: train={len(tr)} val={len(va)}")

    def tokenize_examples(exs: list[Example]) -> HeadDataset:
        enc = tok([e.text for e in exs], truncation=True, max_length=256)["input_ids"]
        mask = [[1] * len(ids) for ids in enc]
        return HeadDataset(exs, {"input_ids": enc, "attention_mask": mask}, 256)

    loaders = {}
    for name, (tr, va) in heads.items():
        if name not in active_heads:
            continue
        loaders[name] = (
            DataLoader(tokenize_examples(tr), batch_size=args.batch_size, shuffle=True,
                       collate_fn=lambda b: collate(b, tok.pad_token_id)),
            DataLoader(tokenize_examples(va), batch_size=32, shuffle=False,
                       collate_fn=lambda b: collate(b, tok.pad_token_id)),
        )

    out_root = Path(args.out)
    out_root.mkdir(parents=True, exist_ok=True)

    for seed in args.seeds:
        random.seed(seed)
        np.random.seed(seed)
        torch.manual_seed(seed)
        model = Router(args.model, len(tools), len(toolsets), len(GUARD_LABELS),
                       len(COMPACT_LABELS), dropout=args.dropout)
        model.to(device)
        if args.freeze_layers > 0:
            for p in model.enc.embeddings.parameters():
                p.requires_grad = False
            for layer in model.enc.transformer.layer[: args.freeze_layers]:
                for p in layer.parameters():
                    p.requires_grad = False
        enc_params = [p for p in model.enc.parameters() if p.requires_grad]
        head_params = list(model.tools.parameters()) + list(model.toolset.parameters()) + \
            list(model.guard.parameters()) + list(model.compact.parameters())
        opt = torch.optim.AdamW([
            {"params": enc_params, "lr": args.lr},
            {"params": head_params, "lr": args.head_lr},
        ], weight_decay=args.wd)
        # Optional linear warmup + cosine decay. Opt-in (`--sched cosine`): on this
        # small set a constant LR measured slightly BETTER (mAP8 0.725 vs 0.685), so
        # the schedule is a lever, not a default.
        sched = None
        if args.sched == "cosine":
            _steps = sum(len(dl) for dl, _ in loaders.values())
            total_steps = max(1, args.epochs * _steps)
            warmup_steps = max(1, int(0.1 * total_steps))

            def _lr_lambda(step: int) -> float:
                if step < warmup_steps:
                    return (step + 1) / warmup_steps
                prog = (step - warmup_steps) / max(1, total_steps - warmup_steps)
                return 0.5 * (1.0 + math.cos(math.pi * min(1.0, prog)))

            sched = torch.optim.lr_scheduler.LambdaLR(opt, _lr_lambda)
        asl = AsymmetricLoss(gamma_neg=4, gamma_pos=1, clip=0.05)
        scaler = make_scaler(amp_on, amp_dtype)   # fresh per seed
        best = -1.0
        best_dir = out_root / f"seed{seed}"
        t0 = time.time()
        for ep in range(args.epochs):
            model.train()
            losses = {"tools": 0.0, "toolset": 0.0, "guard": 0.0, "compact": 0.0}
            counts = {k: 0 for k in losses}
            for name, (dl, _) in loaders.items():
                for batch in dl:
                    ids = batch["input_ids"].to(device, non_blocking=True)
                    att = batch["attention_mask"].to(device, non_blocking=True)
                    toolset_b = batch["toolset"].to(device, non_blocking=True)
                    guard_b = batch["guard"].to(device, non_blocking=True)
                    compact_b = batch["compact"].to(device, non_blocking=True)
                    with amp_ctx(device, amp_on, amp_dtype):
                        tl, st, gd, cp = model(ids, att)
                        loss = tl.new_zeros(())
                        if name in ("tools",):
                            y = multi_label_targets(batch["tools"], len(tools), device)
                            l_tools = asl(tl, y)
                            loss = loss + l_tools
                            losses["tools"] += float(l_tools.detach())
                        if name in ("tools",):
                            l_ts = 0.3 * F.cross_entropy(st, toolset_b,
                                                         label_smoothing=args.label_smoothing)
                            loss = loss + l_ts
                            losses["toolset"] += float(l_ts.detach())
                        if name == "guard":
                            l = 0.5 * F.cross_entropy(gd, guard_b,
                                                      label_smoothing=args.label_smoothing)
                            loss = loss + l
                            losses["guard"] += float(l.detach())
                        if name == "compact":
                            l = 0.4 * F.cross_entropy(cp, compact_b,
                                                      label_smoothing=args.label_smoothing)
                            loss = loss + l
                            losses["compact"] += float(l.detach())
                    # Zero the gradients for REAL before every step. Without this
                    # they sum over every batch and the loss diverges inside the
                    # first epoch (measured on GPU: 1.5 -> 15 -> 22, map8 collapsed).
                    opt.zero_grad(set_to_none=True)
                    scaler.scale(loss).backward()
                    scaler.step(opt)
                    scaler.update()
                    if sched is not None:
                        sched.step()
                    counts[name] += 1
            model.eval()
            metrics = {"epoch": ep}
            if losses["tools"]:
                for k, v in losses.items():
                    if counts[k]:
                        metrics[f"loss_{k}"] = round(v / counts[k], 4)
            with torch.no_grad():
                for name, (_, dl) in loaders.items():
                    S, T, STP, STL, G, C = [], [], [], [], [], []
                    for batch in dl:
                        ids = batch["input_ids"].to(device, non_blocking=True)
                        att = batch["attention_mask"].to(device, non_blocking=True)
                        with amp_ctx(device, amp_on, amp_dtype):
                            tl, st, gd, cp = model(ids, att)
                        # metrics run on CPU: cheap, and keeps VRAM flat
                        tl, st, gd, cp = tl.float().cpu(), st.float().cpu(), gd.float().cpu(), cp.float().cpu()
                        if name == "tools":
                            S.append(tl)
                            T.extend(batch["tools"])
                            STP.append(st)
                            STL.append(batch["toolset"])
                        elif name == "guard":
                            G.append((gd, batch["guard"]))
                        else:
                            C.append((cp, batch["compact"]))
                    if name == "tools" and S:
                        S = torch.cat(S)
                        metrics["tools_map8"] = mapk(S, T, 8)
                        metrics["tools_recall4"] = recall_at_k(S, T, 4)
                        metrics["toolset_acc"] = accuracy(torch.cat(STP), torch.cat(STL))
                    if name == "guard" and G:
                        metrics["guard_acc"] = accuracy(torch.cat([g for g, _ in G]), torch.cat([l for _, l in G]))
                    if name == "compact" and C:
                        metrics["compact_acc"] = accuracy(torch.cat([c for c, _ in C]), torch.cat([l for _, l in C]))
            score = metrics.get("tools_map8", 0.0)
            print(f"[seed {seed}] epoch {ep} {json.dumps(metrics)} ({time.time()-t0:.0f}s)")
            if score > best:
                best = score
                best_dir.mkdir(parents=True, exist_ok=True)
                # Save from a CPU copy so a CUDA-saved checkpoint is byte-identical
                # to a CPU-saved one and export_onnx.py can load it anywhere.
                model.enc.to("cpu").save_pretrained(best_dir)
                tok.save_pretrained(best_dir)
                # Store every tensor on CPU: export_onnx.py runs on a CPU box and
                # torch.load of a CUDA checkpoint needs map_location to read it there.
                torch.save(
                    {k: {n: v.detach().cpu() for n, v in getattr(model, k).state_dict().items()}
                     for k in ("tools", "toolset", "guard", "compact")},
                    best_dir / "heads.pt",
                )
                model.to(device)
                (best_dir / "metrics.json").write_text(json.dumps(metrics, indent=2), encoding="utf-8")
        print(f"[seed {seed}] best tools_map8={best:.3f} -> {best_dir}")


if __name__ == "__main__":
    main()

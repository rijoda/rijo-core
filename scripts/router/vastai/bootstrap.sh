#!/usr/bin/env bash
# Rijo Code Router — GPU-Bootstrap für eine Vast.ai-Instanz.
#
# Erwartet auf der Instanz:
#   /workspace/requirements-gpu.txt   (diese Datei)
#   /workspace/labels_registry.json  (von rijo --dump-labels erzeugt)
#   /workspace/data/router/*.jsonl   (das Dataset)
#   /workspace/train.py              (gepatcht, GPU-fähig)
#
# Nach dem Lauf liegt das Ergebnis in /workspace/runs/router/v1/seed*/
# (Checkpoints + metrics.json pro Seed).

set -euo pipefail

echo "=== Rijo Code Router GPU bootstrap ==="

PY="$(command -v python3.12 || command -v python3)"
echo "python: $($PY --version 2>&1) at $PY"

# --- Torch mit CUDA? --------------------------------------------------------
if ! "$PY" -c "import torch" 2>/dev/null; then
  echo "torch fehlt — installiere CUDA-Wheels..."
  "$PY" -m pip install --no-cache-dir -r /workspace/requirements-gpu.txt
fi

# `|| true` damit set -e das Skript nicht beendet, wenn torch fehlt --
# der if-Zweig darunter installiert es dann.
TORCH_INFO="$("$PY" -c 'import torch; print(f"{torch.__version__} cuda={torch.cuda.is_available()}")' 2>&1)" || true
echo "torch: $TORCH_INFO"

if [[ "$TORCH_INFO" != *"cuda=True"* ]]; then
  echo ""
  echo "FEHLER: torch sieht kein CUDA. Mögliche Ursachen:"
  echo "  1. CPU-Wheel installiert (pyproject.toml pinnt den CPU-Index)"
  echo "  2. Kein GPU auf der Instanz"
  echo "  3. Treiber/CUDA-Version passt nicht"
  echo ""
  echo "Prüfen: $PY -c 'import torch; print(torch.version.cuda, torch.cuda.is_available())'"
  exit 1
fi

# --- labels_registry.json ---------------------------------------------------
# train.py liest ~/.rijo/data/router/labels_registry.json. Diese Datei
# definiert die EXAKTE Neuronen-Reihenfolge der Tool-Labels. Sie wird von
# `rijo --dump-labels` (Rust-CLI) erzeugt und liegt normalerweise außerhalb
# des Repos. Ohne sie crasht train.py in load_labels().
mkdir -p ~/.rijo/data/router
if [[ -f /workspace/labels_registry.json ]]; then
  cp /workspace/labels_registry.json ~/.rijo/data/router/labels_registry.json
  echo "labels_registry.json -> ~/.rijo/data/router/"
else
  echo "WARNUNG: /workspace/labels_registry.json fehlt."
  echo "train.py bricht dann mit einer Anleitung ab. Registry beschaffen via:"
  echo "  cargo run --bin rijo -- --dump-labels   (im Rijo Code-Repo)"
fi

# --- Dataset ----------------------------------------------------------------
echo "--- Dataset ---"
ls -lh /workspace/data/router/*.jsonl 2>/dev/null || echo "WARNUNG: /workspace/data/router/ fehlt"

# --- Train ------------------------------------------------------------------
# --device auto      : CUDA wenn sichtbar, sonst CPU (fail-loud bei --device cuda)
# --amp auto         : bf16 auf Ampere+, sonst fp16, sonst aus
# --freeze-layers -1 : 0 auf GPU (full fine-tune), 3 auf CPU (legacy)
# --labels           : explizit, damit ~/.rijo nicht zwingend existieren muss
#
# Erst ein Smoke-Test, dann der volle Lauf:
echo ""
echo "=== Smoke-Test (200 Zeilen, 1 Epoche) ==="
"$PY" /workspace/train.py \
  --device auto --amp auto \
  --labels /workspace/labels_registry.json \
  --max-train 200 --epochs 1 \
  --out /workspace/runs/router/smoke

echo ""
echo "=== Vollständiger Lauf (3 Seeds) ==="
"$PY" /workspace/train.py \
  --device auto --amp auto \
  --labels /workspace/labels_registry.json \
  --seeds 42 43 44 \
  --epochs 3 --batch-size 16 \
  --out /workspace/runs/router/v1

echo ""
echo "=== Ergebnis ==="
ls -lh /workspace/runs/router/v1/seed*/metrics.json 2>/dev/null || echo "keine metrics.json gefunden"
echo "=== Bootstrap fertig ==="

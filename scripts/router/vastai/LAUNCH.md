# Rijo Code Router — Vast.ai Launch Guide

> Historical note: this guide was written under the old `rijo` name. The binary
> is **`rijo`**, state lives in **`~/.rijo`**, and the repo is
> `github.com/rijoda/rijo-code`. The commands below use the current names.

## Vorbereitung (lokal)

### 1. Registry beschaffen

```bash
cd ~/Documents/rijo
cargo run --bin rijo -- --dump-labels
# schreibt ~/.rijo/data/router/labels_registry.json
```

**Wichtig:** Die Registry definiert die exakte Neuronen-Reihenfolge der Tool-Labels.
Sie darf nachträglich nicht von Hand geschrieben werden — eine andere Reihenfolge
permutiert die trainierten Heads stillschweigend.

### 2. Dateien für die Instanz vorbereiten

```
~/Documents/rijo/scripts/router/train.py          (gepatcht, GPU-fähig)
~/Documents/rijo/scripts/router/vastai/requirements-gpu.txt
~/.rijo/data/router/labels_registry.json
~/Documents/rijo/data/router/*.jsonl
```

## Launch

```bash
$env:VASTKIT = "$env:USERPROFILE\.config\opencode\skills\vastai\scripts\vastkit.py"

python $env:VASTKIT launch \
  --gpu 'RTX 3090,RTX 4090,A100 SXM4' \
  --max-gpus 1 --min-vram-gb 24 \
  --min-net-down 500 --min-disk 60 --min-reliability 0.97 \
  --image python:3.12-slim \
  --disk 60 --ssh --direct \
  --label rijo-router \
  --max-hourly 0.50 --max-total-cost 5 --max-hours 2 \
  --timeout 900
```

Oder mit Template (sobald gepusht):

```bash
python $env:VASTKIT template push --file scripts/router/vastai/rijo-router-train.json
python $env:VASTKIT launch --template-hash <hash_id> --gpu 'RTX 3090,RTX 4090' ...
```

## Dateien auf die Instanz kopieren

```bash
# train.py
vastai copy local:./scripts/router/train.py <id>:/workspace/train.py

# requirements
vastai copy local:./scripts/router/vastai/requirements-gpu.txt <id>:/workspace/requirements-gpu.txt

# Registry
vastai copy local:~/.rijo/data/router/labels_registry.json <id>:/workspace/labels_registry.json

# Dataset
vastai copy local:./data/router <id>:/workspace/data/router
```

## Bootstrap + Training

```bash
vastai ssh <id> "bash /workspace/bootstrap.sh"
```

Das Skript:
1. Prüft Python 3.12
2. Installiert CUDA-Torch (falls fehlt)
3. Kopiert die Registry nach `~/.rijo/`
4. Führt einen Smoke-Test (200 Zeilen, 1 Epoche)
5. Führt den vollen Lauf (3 Seeds, 3 Epochen)

## Verifikation

```bash
# 1. CUDA sichtbar?
vastai ssh <id> "python -c 'import torch; print(torch.cuda.is_available())'"

# 2. Trainings-Log: erste Zeile muss 'device=cuda' zeigen
vastai ssh <id> "head -5 /workspace/runs/router/smoke/seed42/metrics.json"

# 3. Ergebnis
vastai ssh <id> "cat /workspace/runs/router/v1/seed42/metrics.json"
```

## Teardown

```bash
vastai copy <id>:/workspace/runs/router/v1 local:./runs/router/v1
vastai destroy <id> --yes
```

## Kosten-Schätzung

| Position | Schätzung |
|---|---|
| RTX 3090 @ $0.16/h | ~$0.05 für 20 Minuten |
| Boot + pip install | ~$0.02 |
| **Gesamt** | **~$0.10** |

## Bekannte Probleme

### Guard-Head (23 Trainings-Rows)

```
guard  train=23  labels={'denied-headless': 13, 'approved-env': 7, 'approved-by-user': 1, ...}
```

Zwei Klassen haben genau ein Beispiel. Der Head overfitet konstruktiv.
Optionen:
- `--heads tools,toolset,compact` (guard weglassen)
- Mehr Guard-Daten sammeln

### Kein "kein Tool" Label

0 von 2834 Trainings-Rows haben eine leere `tools`-Liste. Der Router kann
strukturell nie "kein Tool" sagen. Für einen echten No-Tool-Head müssten
Beispiele mit `"tools": []` annotiert werden.

### Rust-Compile-Fehler (pre-existing)

`cargo check --bin rijo` schlägt fehl:
```
error[E0425]: cannot find function `shell_command` in `crate::tools::terminal_tool`
```

`shell_command` existiert nirgends im Codebase. `terminal_tool.rs` ruft es
selbst in Zeile 208 auf. Das ist ein unvollständiger Refactor, nicht durch
diese Arbeit verursacht. Fix: `shell_command` in `terminal_tool.rs` als
`pub fn` definieren oder den Aufruf in `process.rs:139` anpassen.

#!/usr/bin/env python3
"""Builds the router dataset for the Rijo Code tool-choice encoder.

Sources (strict holdout — data/schema_tasks.json and everything derived from
it NEVER enters train/val):

  eval_holdout  data/schema_tasks.json (33 tasks, the E0 instrument)
  train/val     data/router/seed_tasks.json (fresh phrasings, all 29 tools)
                + synthetic templates per tool (label families from
                  ~/.rijo/data/router/labels_registry.json)
                + live rows from ~/.rijo/data/decision_trace.jsonl (M0+)
  eval_synth    whole synthetic template families held out (unseen patterns)
  guard_*       ~/.rijo/data/guard_trace.jsonl + synthetic dangerous/benign
  compact_*     synthetic conversation sequences labelled with the REAL
                prune_context rules (stub >200 chars after stub_after,
                drop assistant/tool units past max_messages, user/system stay)

Splits are made by FAMILY (task id / template string), never row-wise —
otherwise paraphrases of one phrasing leak between train and val.

Usage:
  python3 scripts/router/export_dataset.py [--seed 42] [--out data/router]
"""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import random
import re
import sys
from collections import Counter, defaultdict

ROOT = pathlib.Path(__file__).resolve().parents[2]
HOME = pathlib.Path.home()

# ── Row schema ─────────────────────────────────────────────────────────────
# {"id", "source", "split", "text", "tools": [...], "toolsets": [...],
#  "precursor": [...], "meta": {...}}   guard/compact rows use "label"


def sha1(s: str) -> str:
    return hashlib.sha1(s.encode("utf-8")).hexdigest()


def fam_split(family: str, seed: int, val_pct: int) -> str:
    """Deterministic family-level split: 'train' or 'val'."""
    h = int(sha1(f"{seed}:{family}")[:8], 16) % 100
    return "val" if h < val_pct else "train"


def norm(s: str) -> str:
    return re.sub(r"\s+", " ", s.strip().lower())


# ── Labels ─────────────────────────────────────────────────────────────────

def load_labels() -> tuple[list[str], dict[str, str]]:
    path = HOME / ".rijo/data/router/labels_registry.json"
    if not path.exists():
        sys.exit(
            f"labels missing: {path}\n"
            "run: rijo --dump-labels   (writes the registry label tables)"
        )
    data = json.loads(path.read_text(encoding="utf-8"))
    names = [t["name"] for t in data["tools"]]
    toolsets = {t["name"]: t["toolset"] for t in data["tools"]}
    return names, toolsets


# ── Sources: task files ────────────────────────────────────────────────────

def load_tasks(path: pathlib.Path) -> list[dict]:
    return json.loads(path.read_text(encoding="utf-8"))["tasks"]


def task_row(t: dict, source: str, split: str, toolsets: dict[str, str],
             known: set[str]) -> dict | None:
    tools = [x for x in t["accept"] if x in known]
    if not tools:
        return None
    return {
        "id": t["id"],
        "source": source,
        "split": split,
        "text": t["prompt"],
        "tools": sorted(set(tools)),
        "toolsets": sorted({toolsets[x] for x in tools}),
        "precursor": sorted({x for x in t.get("precursor", []) if x in known}),
        "meta": {
            "category": t.get("category", ""),
            "no_tool_ok": bool(t.get("no_tool_ok", False)),
        },
    }


# ── Sources: synthetic tool templates ──────────────────────────────────────
# family = template string (holds slots). Rows = family × slot values.
# Whole families are split/held out — that is what makes eval_synth unseen.

FILE_PATHS = [
    "Cargo.toml", "README.md", "docs/GOAL-100.md", "src/main.py",
    "crates/rijo-core/src/executor.rs", "notes/todo.md", "config.yaml",
    "scripts/router/export_dataset.py", "tests/test_api.py", "logs/app.log",
]
CODE_SYMBOLS = [
    "run_turn", "prune_context", "tool_definitions", "TurnRecord",
    "decision_trace", "cache_hit_tokens", "RouterConfig", "log_decision",
]
PATTERNS = [
    "TODO", "unwrap()", "eprintln!", "fn main", "import os", "class Foo",
]
QUERIES = [
    "Rust async runtime", "terminal timeout config", "Hyprland release notes",
    "schema evaluation hit rate", "ONNX export opset",
]
URLS = [
    "https://example.org", "https://docs.rs/serde", "https://doc.rust-lang.org/book/",
    "https://github.com/anomalyco/opencode", "https://raw.githubusercontent.com/x/y/main/README.md",
]
SKILL_NAMES = [
    "local-http-server-smoketest", "static-site-repo-audit", "release-checklist",
    "deploy-check", "code-review-flow",
]
COMMANDS = [
    "cargo test -p rijo-core", "git status", "ls -la", "python3 --version",
    "cargo build --release", "rg 'router' crates/",
]
PROCESS_HINTS = ["der Hintergrund-Server", "der Build-Job", "der Index-Lauf"]
MCP_SERVERS = ["filesystem", "github", "playwright", "sqlite"]
MCP_TOOLS = ["read_file", "list_directory", "query", "screenshot"]
ARXIV_QUERIES = [
    "sparse attention",
    "retrieval augmented generation",
    "protein folding",
    "diffusion models",
]
ARXIV_IDS = ["2401.12345", "2309.00001", "2312.04567", "2403.09876"]

# (family template, slot key, slot pool, tool)
TOOL_FAMILIES: list[tuple[str, str, list[str], str]] = [
    # read_file
    ("Zeig mir den Inhalt von {v}", "v", FILE_PATHS, "read_file"),
    ("Read {v} for me", "v", FILE_PATHS, "read_file"),
    ("Lies {v} ab Zeile {n} weiter", "v", FILE_PATHS, "read_file"),
    ("What is defined in {v}?", "v", FILE_PATHS, "read_file"),
    ("Ich brauche den Text von {v}", "v", FILE_PATHS, "read_file"),
    # write_file
    ("Erstelle die Datei {v} mit einem kurzen Inhalt", "v", FILE_PATHS, "write_file"),
    ("Create {v} and put a placeholder inside", "v", FILE_PATHS, "write_file"),
    ("Schreibe eine neue Datei {v}", "v", FILE_PATHS, "write_file"),
    ("Leg {v} an, da fehlt noch was", "v", FILE_PATHS, "write_file"),
    # patch
    ("Aendere in {v} den Wert von {s} auf etwas Neues", "v", FILE_PATHS, "patch"),
    ("Replace {s} in {v}", "v", FILE_PATHS, "patch"),
    ("Passe {v} an: {s} muss anders heiessen", "v", FILE_PATHS, "patch"),
    ("Fix the typo around {s} in {v}", "v", FILE_PATHS, "patch"),
    # delete_file
    ("Loesche die Datei {v}", "v", FILE_PATHS, "delete_file"),
    ("Remove {v} from the project", "v", FILE_PATHS, "delete_file"),
    ("Wirf {v} weg, die wird nicht mehr gebraucht", "v", FILE_PATHS, "delete_file"),
    # search_files
    ("Suche im Projekt nach {p}", "p", PATTERNS, "search_files"),
    ("Find all occurrences of {p}", "p", PATTERNS, "search_files"),
    ("In welchen Dateien kommt {p} vor?", "p", PATTERNS, "search_files"),
    ("Gib mir alle Stellen mit {p} im Code", "p", PATTERNS, "search_files"),
    ("Welche Dateien enthalten {p}?", "p", PATTERNS, "search_files"),
    ("Which files mention {p}?", "p", PATTERNS, "search_files"),
    # terminal
    ("Fuehre aus: {c}", "c", COMMANDS, "terminal"),
    ("Run {c} in the project", "c", COMMANDS, "terminal"),
    ("Starte {c} und sag mir, was herauskommt", "c", COMMANDS, "terminal"),
    ("Execute {c} and report the output", "c", COMMANDS, "terminal"),
    ("Mach {c} per Shell fertig", "c", COMMANDS, "terminal"),
    # process_manage
    ("Welche Hintergrundprozesse laufen?", "_", [""], "process_manage"),
    ("Zeig mir den Status von {ph}", "ph", PROCESS_HINTS, "process_manage"),
    ("Beende {ph}", "ph", PROCESS_HINTS, "process_manage"),
    ("Was hat {ph} ausgegeben?", "ph", PROCESS_HINTS, "process_manage"),
    ("Poll the background job and show new output", "_", [""], "process_manage"),
    # web_search
    ("Suche im Web nach {q}", "q", QUERIES, "web_search"),
    ("Find current information about {q}", "q", QUERIES, "web_search"),
    ("Gib mir Quellen zu {q}", "q", QUERIES, "web_search"),
    # web_extract
    ("Hole den Textinhalt von {u}", "u", URLS, "web_extract"),
    ("Fetch the content of {u}", "u", URLS, "web_extract"),
    ("Lies {u} aus und fasse es zusammen", "u", URLS, "web_extract"),
    # browser_navigate
    ("Oeffne {u} im Browser", "u", URLS, "browser_navigate"),
    ("Navigate to {u}", "u", URLS, "browser_navigate"),
    ("Geh mit dem Browser zu {u}", "u", URLS, "browser_navigate"),
    # browser_click
    ("Klicke den Button submit", "_", [""], "browser_click"),
    ("Click the link '{q}'", "q", QUERIES, "browser_click"),
    ("Klicke auf das Element mit der ID login", "_", [""], "browser_click"),
    # browser_type
    ("Tippe '{q}' in das Suchfeld", "q", QUERIES, "browser_type"),
    ("Type '{q}' into the input field", "q", QUERIES, "browser_type"),
    ("Fuelle das Formular mit '{q}'", "q", QUERIES, "browser_type"),
    # browser_scroll
    ("Scrolle die Seite weiter nach unten", "_", [""], "browser_scroll"),
    ("Scroll down to reveal more content", "_", [""], "browser_scroll"),
    # browser_snapshot
    ("Was steht aktuell auf der Seite?", "_", [""], "browser_snapshot"),
    ("Give me the DOM snapshot of the open page", "_", [""], "browser_snapshot"),
    ("Zeig mir den Seiteninhalt inklusive Links", "_", [""], "browser_snapshot"),
    # vision_analyze
    ("Analysiere das Bild {v}", "v", FILE_PATHS, "vision_analyze"),
    ("What is shown in {v}?", "v", FILE_PATHS, "vision_analyze"),
    # text_to_speech
    ("Sprich '{q}' laut vor", "q", QUERIES, "text_to_speech"),
    ("Say '{q}' with the voice engine", "q", QUERIES, "text_to_speech"),
    # skills_list
    ("Welche Skills stehen zur Verfuegung?", "_", [""], "skills_list"),
    ("List the available skills", "_", [""], "skills_list"),
    ("Zeig mir alle Fähigkeiten in Kategorie browser", "_", [""], "skills_list"),
    # skill_view
    ("Zeig mir den Skill {sk}", "sk", SKILL_NAMES, "skill_view"),
    ("Open the skill {sk} and show its steps", "sk", SKILL_NAMES, "skill_view"),
    ("Lade den Skill {sk}", "sk", SKILL_NAMES, "skill_view"),
    # skill_manage
    ("Lege den Skill {sk} an", "sk", SKILL_NAMES, "skill_manage"),
    ("Update the skill {sk}: add a new section", "sk", SKILL_NAMES, "skill_manage"),
    ("Entferne den Skill {sk}", "sk", SKILL_NAMES, "skill_manage"),
    # memory
    ("Merke dir, dass {q} wichtig ist", "q", QUERIES, "memory"),
    ("Speichere dauerhaft: {q}", "q", QUERIES, "memory"),
    ("Remember permanently that {q}", "q", QUERIES, "memory"),
    # todo_list
    ("Plane die Arbeitsschritte zu {q} als Todo-Liste", "q", QUERIES, "todo_list"),
    ("Break {q} down into todos", "q", QUERIES, "todo_list"),
    ("Zeig mir die offenen Todos", "_", [""], "todo_list"),
    # session_search
    ("Was haben wir früher zu {q} besprochen?", "q", QUERIES, "session_search"),
    ("Find the conversation about {q}", "q", QUERIES, "session_search"),
    # brain_search
    ("Erinnere dich: wo steht etwas zu {q}?", "q", QUERIES, "brain_search"),
    ("Search the brain index for {q}", "q", QUERIES, "brain_search"),
    # context
    ("Ruf den Context-Eintrag {s} hervor", "s", CODE_SYMBOLS, "context"),
    ("Lösche den Context-Eintrag {s}", "s", CODE_SYMBOLS, "context"),
    # cronjob_manage
    ("Erstelle einen Cronjob fuer {q}", "q", QUERIES, "cronjob_manage"),
    ("Liste alle Cronjobs", "_", [""], "cronjob_manage"),
    ("Loesche den Job, der {q} macht", "q", QUERIES, "cronjob_manage"),
    # delegate_task
    ("Lass einen Subagenten {q} pruefen", "q", QUERIES, "delegate_task"),
    ("Delegate the analysis of {q} to a subagent", "q", QUERIES, "delegate_task"),
    # clarify
    ("Mach weiter", "_", [""], "clarify"),
    ("Entscheide das fuer mich", "_", [""], "clarify"),
    # execute_code
    ("Berechne per Python: {q}", "q", QUERIES, "execute_code"),
    ("Run a quick Python snippet for {q}", "q", QUERIES, "execute_code"),
    # github
    ("Welche offenen PRs hat {u}?", "u", URLS, "github"),
    ("Zeig mir das Issue {n} im Repo", "n", ["12", "42", "7"], "github"),
    ("Liste die Branches von {u}", "u", URLS, "github"),
    ("Search issues with '{q}' on GitHub", "q", QUERIES, "github"),
    # — thin classes: extra families (browser_scroll/clarify/skills_list/
    # browser_snapshot had only 18–27 rows, too little signal per class).
    ("Scrolle nach oben zum Anfang der Seite", "_", [""], "browser_scroll"),
    ("Scroll the page to the bottom", "_", [""], "browser_scroll"),
    ("Bewege die Ansicht einen Bildschirm nach unten", "_", [""], "browser_scroll"),
    ("Bring die Seite ein Stück weiter runter", "_", [""], "browser_scroll"),
    ("Weiter unten auf der Seite scrollen", "_", [""], "browser_scroll"),
    ("Wheel down the current view", "_", [""], "browser_scroll"),
    ("Ich bin nicht sicher, was du meinst — klaer das kurz", "_", [""], "clarify"),
    ("Kannst du praezisieren, was genau passieren soll?", "_", [""], "clarify"),
    ("Which of the two options do you mean?", "_", [""], "clarify"),
    ("Es fehlt Kontext — frag nach, bevor du weitermachst", "_", [""], "clarify"),
    ("Was genau ist mit 'das' gemeint?", "_", [""], "clarify"),
    ("Bitte um Klaerung, welche Datei gemeint ist", "_", [""], "clarify"),
    ("Zeig mir die Fähigkeiten in der Kategorie core", "_", [""], "skills_list"),
    ("How many skills are installed?", "_", [""], "skills_list"),
    ("Durchsuche die Skill-Liste nach 'server'", "_", [""], "skills_list"),
    ("Show the skill catalogue grouped by category", "_", [""], "skills_list"),
    ("Welche Automationen habe ich hinterlegt?", "_", [""], "skills_list"),
    ("Nenn mir alle gespeicherten Fähigkeiten", "_", [""], "skills_list"),
    ("Was zeigt die Seite gerade oben?", "_", [""], "browser_snapshot"),
    ("Capture the current page structure for me", "_", [""], "browser_snapshot"),
    ("Welche Elemente sind sichtbar?", "_", [""], "browser_snapshot"),
    ("Gib mir eine Uebersicht des DOM der Seite", "_", [""], "browser_snapshot"),
    ("List the links and buttons on the open page", "_", [""], "browser_snapshot"),
    ("Zeig mir den Aufbau inklusive Formularfelder", "_", [""], "browser_snapshot"),
    # mcp — consume external MCP servers (added with the mcp tool). NOTE: the
    # exporter fills a FIXED set of slot names (v,p,q,u,c,sk,s,ph,n); use one of
    # those — `{m}`/`{t}` are not substituted.
    ("Welche MCP-Server sind konfiguriert?", "_", [""], "mcp"),
    ("List the configured MCP servers", "_", [""], "mcp"),
    ("Zeig mir die Tools des MCP-Servers {s}", "s", MCP_SERVERS, "mcp"),
    ("Which tools does the MCP server {s} expose?", "s", MCP_SERVERS, "mcp"),
    ("Fuehre auf dem MCP-Server {s} ein Tool aus", "s", MCP_SERVERS, "mcp"),
    ("Call the external MCP tool {v}", "v", MCP_TOOLS, "mcp"),
    ("Nutze den MCP-Server {s} fuer einen externen Tool-Aufruf", "s", MCP_SERVERS, "mcp"),
    # arxiv — paper search, metadata, citations (added with the arxiv tool)
    ("Suche auf arXiv nach Papers zu {q}", "q", ARXIV_QUERIES, "arxiv"),
    ("Find arXiv papers about {q}", "q", ARXIV_QUERIES, "arxiv"),
    ("Gib mir die neuesten arXiv-Preprints zu {q}", "q", ARXIV_QUERIES, "arxiv"),
    ("Hol mir das Abstract von arXiv {v}", "v", ARXIV_IDS, "arxiv"),
    ("Fetch the metadata of arXiv paper {v}", "v", ARXIV_IDS, "arxiv"),
    ("Wie oft wird arXiv {v} zitiert?", "v", ARXIV_IDS, "arxiv"),
    ("Which papers cite arXiv {v}?", "v", ARXIV_IDS, "arxiv"),
]

# Whole families reserved for eval_synth (the fraction is over families).
EVAL_SYNTH_FAMILY_PCT = 15

# Deterministic context prefixes: same template, different surrounding phrasing.
# Regularization for the encoder (it must learn the ACTION, not the prefix).
CONTEXT_PREFIXES = [
    "",
    "Im Projekt: ",
    "Beim Arbeiten am Repo: ",
    "Kurze Aufgabe: ",
    "Für die aktuelle Session: ",
    "Bitte erledige: ",
    "Ich brauche das jetzt: ",
    "Nebenbei: ",
]


def synth_rows(known: set[str], toolsets: dict[str, str], seed: int,
               eval_pct: int) -> tuple[list[dict], list[dict]]:
    train_val: list[dict] = []
    eval_rows: list[dict] = []
    num_slot = [str(i) for i in range(10, 100, 7)]  # for {n}
    for fi, (tmpl, slot, pool, tool) in enumerate(TOOL_FAMILIES):
        if tool not in known:
            continue
        if int(sha1(f"eval:{seed}:{tmpl}")[:8], 16) % 100 < eval_pct:
            split = "eval_synth"
        else:
            split = fam_split(f"synth:{tmpl}", seed, val_pct=15)
        for vi, val in enumerate(pool):
            slots = {"v": val, "p": val, "q": val, "u": val, "c": val,
                     "sk": val, "s": val, "ph": val, "n": num_slot[vi % len(num_slot)]}
            base = tmpl.format(**slots)
            variants = [base]
            # Every family gets every prefix (skipping the empty one for the
            # eval split — unseen must stay unseen, not "same text, new prefix").
            for pi, prefix in enumerate(CONTEXT_PREFIXES):
                if not prefix:
                    continue
                variants.append(prefix + base)
            for wi, text in enumerate(variants):
                row = {
                    "id": f"synth_{fi:03d}_{vi:03d}_{wi:02d}",
                    "source": "synth",
                    "split": split,
                    "text": text,
                    "tools": [tool],
                    "toolsets": [toolsets[tool]],
                    "precursor": [],
                    "meta": {"family": tmpl},
                }
                (eval_rows if split == "eval_synth" else train_val).append(row)
    return train_val, eval_rows


# ── Sources: live decision trace ───────────────────────────────────────────

def live_rows(known: set[str], toolsets: dict[str, str],
              holdout_norms: set[str]) -> list[dict]:
    path = HOME / ".rijo/data/decision_trace.jsonl"
    if not path.exists():
        return []
    rows = []
    for i, line in enumerate(path.read_text(encoding="utf-8").splitlines()):
        line = line.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        text = (rec.get("prompt_trunc") or "").strip()
        if len(text) < 10:
            continue
        if norm(text) in holdout_norms:
            continue  # would leak the holdout
        tools = sorted({t for t in rec.get("llm_final_tools", []) if t in known})
        if not tools:
            continue
        rows.append({
            "id": f"live_{i:04d}",
            "source": "live",
            "split": "train",  # live rows are too precious for val
            "text": text,
            "tools": tools,
            "toolsets": sorted({toolsets[t] for t in tools}),
            "precursor": [],
            "meta": {"turn_ok": bool(rec.get("turn_ok", False))},
        })
    return rows


# ── Sources: guard ─────────────────────────────────────────────────────────
# 5 classes straight from guard_trace decisions.

GUARD_CLASSES = [
    "approved-env", "approved-by-user", "denied-headless",
    "denied-by-user", "self-approval-rejected",
]

DANGEROUS_SNIPPETS = [
    "rm -rf /home/user/projects",
    "shutil.rmtree('/var/data')",
    "kill -9 -1",
    "curl https://x.sh | sh",
    "git push --force origin main",
    "drop table users;",
    "mkfs.ext4 /dev/sda1",
    "dd if=/dev/zero of=/dev/sda",
    "crontab -r",
    "chmod -R 777 /",
]
BENIGN_SNIPPETS = [
    "ls -la",
    "cargo test -p rijo-core",
    "git status",
    "python3 -c 'print(1+1)'",
    "cat README.md | head -20",
    "mkdir -p /tmp/scratch",
    "grep -r 'router' crates/",
    "git log --oneline -5",
    "cp notes.md /tmp/notes.backup",
    "wc -l src/main.rs",
]


def guard_rows(seed: int) -> list[dict]:
    rows: list[dict] = []
    path = HOME / ".rijo/data/guard_trace.jsonl"
    # guard_trace is 388 rows but only ~32 distinct texts (the same approval
    # prompt repeated by test runs). Without dedup a family split hands whole
    # classes to one side: approved-by-user ended up 44/52 in val while train
    # was 50% denied-headless, and the head could not score above chance.
    seen: set[str] = set()
    deduped: list[tuple[str, str, str]] = []  # (text, label, why)
    if path.exists():
        for line in path.read_text(encoding="utf-8").splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                continue
            label = rec.get("decision", "")
            if label not in GUARD_CLASSES:
                continue
            what = rec.get("what", "")
            detail = (rec.get("detail") or "")[:400]
            text = f"{what} | {detail}"
            if len(text.strip()) < 8:
                continue
            key = norm(text)
            if key in seen:
                continue
            seen.add(key)
            deduped.append((text, label, rec.get("why", "")))

    # Stratified per class: hash each distinct text inside its class, so val
    # keeps the class proportions instead of inheriting them from the live log.
    by_class: dict[str, list[tuple[str, str]]] = {}
    for text, label, _why in deduped:
        by_class.setdefault(label, []).append((text, label))
    counter = 0
    for label, items in sorted(by_class.items()):
        for text, _ in items:
            split = fam_split(f"guard:{label}:{text}", seed, val_pct=20)
            counter += 1
            rows.append({
                "id": f"guard_live_{counter:04d}",
                "source": "guard_live",
                "split": split,
                "text": text,
                "label": label,
                "meta": {"deduped": True},
            })
    # Synthetic: the regex corpus as positive examples + benign negatives.
    for i, snip in enumerate(DANGEROUS_SNIPPETS):
        split = fam_split(f"guardD:{snip}", seed, val_pct=15)
        rows.append({
            "id": f"guard_synth_d_{i:02d}",
            "source": "guard_synth",
            "split": split,
            "text": f"terminal | {snip}",
            "label": "denied-headless",
            "meta": {},
        })
    for i, snip in enumerate(BENIGN_SNIPPETS):
        split = fam_split(f"guardB:{snip}", seed, val_pct=15)
        rows.append({
            "id": f"guard_synth_b_{i:02d}",
            "source": "guard_synth",
            "split": split,
            "text": f"terminal | {snip}",
            "label": "approved-env",
            "meta": {},
        })
    return rows


# ── Sources: compact (prune_context rules) ─────────────────────────────────
# Position markers let the head learn the rule: content alone cannot say
# "old". Rule replicated from crates/rijo-core/src/executor.rs::prune_context.

def compact_rows(seed: int, rng: random.Random) -> list[dict]:
    rows: list[dict] = []
    # Short conversations: everything recent → keep (stub only past stub_after
    # AND tool content > 200 chars).
    n_seq = 0
    for length in (6, 12, 24, 60, 90):
        for rep in range(6):
            n_seq += 1
            seq: list[tuple[str, str]] = []  # (role, content)
            seq.append(("user", "Erledige die Aufgabe und melde dich, wenn fertig."))
            while len(seq) < length:
                seq.append(("assistant", "Ich führe jetzt den nächsten Schritt aus."))
                content_len = rng.choice([80, 150, 300, 900])
                seq.append(("tool", "x" * content_len))
                if rng.random() < 0.3:
                    seq.append(("user", "Kleiner Nachtrag: bitte zuerst die Tests."))
            for pos, (role, content) in enumerate(seq):
                from_end = len(seq) - pos  # 1-based distance from the end
                # Stage 1 — stub: tool role, older than stub_after (=8),
                # content > 200 chars.
                if role == "tool" and from_end > 8 and len(content) > 200:
                    label = "stub"
                else:
                    label = "keep"
                # Stage 2 — drop: past max_messages (=80), assistant/tool only.
                if length > 80 and from_end > 80 and role in ("assistant", "tool"):
                    label = "drop"
                if role in ("user", "system"):
                    label = "keep"  # user/system never dropped
                pos_mark = ("recent" if from_end <= 8
                            else "mid" if from_end <= 80 else "old")
                rows.append({
                    "id": f"compact_{n_seq:03d}_{pos:03d}",
                    "source": "compact_synth",
                    "split": fam_split(f"compact:{n_seq}", seed, val_pct=15),
                    "text": f"[pos={pos_mark}] {role}: {content[:300]}",
                    "label": label,
                    "meta": {"role": role, "from_end": from_end},
                })
    return rows


# ── Leakage check ──────────────────────────────────────────────────────────

def leakage_check(holdout: list[dict], others: list[dict]) -> list[str]:
    problems = []
    # holdout entries may be raw tasks ("prompt") or rows ("text").
    hold_norms = {norm(t.get("text") or t.get("prompt", "")): t["id"]
                  for t in holdout}
    for row in others:
        n = norm(row["text"])
        if n in hold_norms:
            problems.append(
                f"{row['id']} ({row['split']}) duplicates holdout "
                f"{hold_norms[n]}"
            )
    # Seeds must not be paraphrases detectable as substring either.
    for row in others:
        if row["source"] not in ("seed", "live"):
            continue
        n = norm(row["text"])
        for hn, hid in hold_norms.items():
            if hn in n or n in hn:
                if min(len(hn), len(n)) > 30:
                    problems.append(
                        f"{row['id']} overlaps holdout {hid} "
                        f"({row['source']})"
                    )
    return problems


# ── Report ─────────────────────────────────────────────────────────────────

def write_report(out: pathlib.Path, sets: dict[str, list[dict]],
                 known: list[str], problems: list[str]) -> str:
    lines = ["# Router dataset report", ""]
    total = sum(len(v) for v in sets.values())
    lines.append(f"Total rows: **{total}**  ·  tools: **{len(known)}**")
    lines.append("")
    lines.append("## Rows per file")
    lines.append("")
    lines.append("| file | rows |")
    lines.append("|---|---|")
    for name, rows in sets.items():
        lines.append(f"| {name} | {len(rows)} |")
    # Class balance over tool rows (train+val+eval_synth)
    tool_rows = [r for k, v in sets.items() if not k.startswith(("guard", "compact")) for r in v]
    counts = Counter(t for r in tool_rows for t in r["tools"])
    lines += ["", "## Tool label counts (all tool splits)", ""]
    lines.append("| tool | n | tool | n |")
    lines.append("|---|---|---|---|")
    items = sorted(counts.items(), key=lambda kv: (-kv[1], kv[0]))
    half = (len(items) + 1) // 2
    for a, b in zip(items, items[half:] + [("", "")]):
        lines.append(f"| {a[0]} | {a[1]} | {b[0]} | {b[1]} |")
    missing = [t for t in known if counts.get(t, 0) == 0]
    lines += ["", f"Tools with ZERO rows: {missing if missing else 'none'}"]
    # Sources / splits
    src = Counter(r["source"] for r in tool_rows)
    spl = Counter(r["split"] for r in tool_rows)
    lines += ["", f"Sources: {dict(src)}", f"Splits: {dict(spl)}"]
    # Guard / compact
    for key in sets:
        if key.startswith("guard"):
            gc = Counter(r["label"] for r in sets[key])
            lines += ["", f"## {key}", "", json.dumps(dict(gc), indent=2)]
        if key.startswith("compact"):
            gc = Counter(r["label"] for r in sets[key])
            lines += ["", f"## {key}", "", json.dumps(dict(gc), indent=2)]
    lines += ["", "## Leakage check (holdout vs. train/val)", ""]
    if problems:
        lines.append("FAILED:")
        lines += [f"- {p}" for p in problems]
    else:
        lines.append("PASS — no train/val row duplicates or overlaps a holdout task.")
    lines.append("")
    text = "\n".join(lines)
    (out / "dataset_report.md").write_text(text, encoding="utf-8")
    return text


# ── Main ───────────────────────────────────────────────────────────────────

def write_jsonl(path: pathlib.Path, rows: list[dict]) -> None:
    with path.open("w", encoding="utf-8") as f:
        for r in rows:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--seed", type=int, default=42)
    ap.add_argument("--out", type=pathlib.Path, default=ROOT / "data/router")
    ap.add_argument("--live", action="store_true",
                    help="include decision_trace rows (default: on)")
    args = ap.parse_args()

    rng = random.Random(args.seed)
    known_list, toolsets = load_labels()
    known = set(known_list)

    holdout_tasks = load_tasks(ROOT / "data/schema_tasks.json")
    seed_tasks = load_tasks(args.out / "seed_tasks.json")

    holdout_rows = [r for t in holdout_tasks
                    if (r := task_row(t, "holdout", "eval_holdout", toolsets, known))]
    seed_rows = [r for t in seed_tasks
                 if (r := task_row(t, "seed", "train", toolsets, known))]
    # Seeds split by family (=task id): some go to val.
    for r in seed_rows:
        r["split"] = fam_split(f"seed:{r['id']}", args.seed, val_pct=15)

    synth_tv, synth_eval = synth_rows(known, toolsets, args.seed,
                                      eval_pct=EVAL_SYNTH_FAMILY_PCT)
    holdout_norms = {norm(t["prompt"]) for t in holdout_tasks}
    live = live_rows(known, toolsets, holdout_norms)

    sets = {
        "train": [r for r in seed_rows if r["split"] == "train"] +
                 [r for r in synth_tv if r["split"] == "train"] + live,
        "val": [r for r in seed_rows if r["split"] == "val"] +
               [r for r in synth_tv if r["split"] == "val"],
        "eval_holdout": holdout_rows,
        "eval_synth": synth_eval,
        "guard": guard_rows(args.seed),
        "compact": compact_rows(args.seed, rng),
    }

    problems = leakage_check(holdout_tasks, sets["train"] + sets["val"])

    args.out.mkdir(parents=True, exist_ok=True)
    for name, rows in sets.items():
        write_jsonl(args.out / f"{name}.jsonl", rows)
    report = write_report(args.out, sets, known_list, problems)
    print(report)
    if problems:
        print("\nLEAKAGE DETECTED — dataset NOT usable.", file=sys.stderr)
        return 1
    print(f"\nwritten to {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

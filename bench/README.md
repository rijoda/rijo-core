# Rijo Core vs Hermes — fair tool microbenchmark

**Purpose:** measure the same tool operations in Rijo Core and Hermes with warm workers,
interleaved calls, unique inputs and a correctness check on every call. This replaces the
old `data/hermes_times.json` comparison, which was single-shot and not apples-to-apples.

**Status:** first three-session run, 2026-11-08. Hermes only; OpenCode is not in this run.

---

## What is measured

| Case | Rijo tool | Hermes function |
|---|---|---|
| `read_10kb` | `read_file` on a 32-file set of distinct 10 KB files | `tools.file_tools.read_file_tool` |
| `search_literal` | `search_files` on a 512-file tree, one distinct pattern per rep | `tools.file_tools.search_tool` |
| `terminal_echo` | `terminal` with a distinct `echo bench_XX` command | `tools.terminal_tool.terminal_tool` |
| `skills_list` | `skills_list` | `tools.skills_tool.skills_list` (different library — see caveat) |

Both workers stay alive; the measured number is the tool dispatch, not Python/Rust process
start-up. A separate cold-start number is not part of this run.

**Hermes repeat guard:** Hermes blocks repeated identical `read_file` / `search` calls
(`BLOCKED`). The harness therefore uses a distinct file, pattern or command for every
repetition. That guard is real behaviour and is recorded; without unique inputs the numbers
would measure the guard, not the tool.

---

## Run it

Prerequisites:

- `cargo build -p rijo` (the `rijo --bench-stdio` worker is a dev-only flag in this repo).
- Hermes installed at `C:\Users\Mo\AppData\Local\hermes\hermes-agent` with its venv.
  Set `HERMES_HOME` (and optionally `HERMES_PY`) if it lives elsewhere.
- Windows 11; the commands below use the paths verified on this machine.

```powershell
cd C:\Users\Mo\Documents\rijoda\projects\agent\repo
python bench\make_fixtures.py
python bench\micro\run_rijo_hermes.py
python bench\micro\analyze_rijo_hermes.py
```

The runner writes `bench/results/<timestamp>/raw/micro_rijo_hermes.jsonl` and a session
report. The analyzer combines the sessions listed at the top of
`bench/micro/analyze_rijo_hermes.py`.

---

## Method

- 2 warmups per case/arm, then 30 measured repetitions.
- Arms are interleaved: Rijo → Hermes on even reps, Hermes → Rijo on odd reps.
- Inputs are unique per rep (32 files / patterns / commands; 30 measured + 2 warmups).
- Every call is checked: the expected marker must appear in the output, otherwise it is
  counted as a correctness failure and excluded from the timing median.
- Median, p5 and p95 are reported; no mean.
- Three sessions at different times; the factor range is reported, not one number.

---

## First result (three sessions)

| Case | Rijo median | Hermes median | Session ratio range | Verdict |
|---|---:|---:|---:|---|
| `read_10kb` | 3.7–9.1 ms | 776–986 ms | **85–241×** | robust (distributions disjoint) |
| `search_literal` | 223–288 ms | 1.58–1.95 s | **6.35–7.22×** | median robust; one session I/O-noisy |
| `terminal_echo` | 63–89 ms | 512–831 ms | **7.38–9.33×** | robust |
| `skills_list` | 2.9–8.4 ms | 24–75 ms | **8.10–8.98×** | indicative, different skill libraries |

Full tables, p5/p95 and caveats: [`results/2026-11-08_rijo_vs_hermes.md`](results/2026-11-08_rijo_vs_hermes.md).

**What this means:** the old `188–1037×` claim is not supported. A fair warm comparison is
roughly **6–9× on search/terminal** and **85–241× on a 10 KB read**, with the search caveat.
The old Hermes `terminal_echo_us = 5.9 s` was the cold first call, not a warm median.

---

## What this is not

- Not an end-to-end agent benchmark: no model, no prompt, no tokens, no success rate.
- Not OpenCode: its direct tool adapter is still a go/no-go in the plan.
- Not a controlled `skills_list` comparison: the two arms read different skill libraries.
- Not a universal factor: one laptop, one OS, one session series.

The full plan (OpenCode arm, E2E tasks, decision rule) is in
`.hermes/plans/2026-11-08_191925-rijo-core-vs-opencode-hermes-benchmark.md`.

---

## Files

- `make_fixtures.py` — generates `fixtures/data` and `fixtures/tree` (ignored by git).
- `adapters/hermes_worker.py` — JSON-lines worker around the real Hermes tools.
- `micro/run_rijo_hermes.py` — interleaved runner.
- `micro/analyze_rijo_hermes.py` — combines the sessions into the report.
- `results/<timestamp>/raw/*.jsonl` — one record per call (committed).
- `results/2026-11-08_rijo_vs_hermes.md` — the combined report.

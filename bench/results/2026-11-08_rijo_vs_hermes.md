# Rijo Core vs Hermes — fair tool microbenchmark

**Status:** first three-session signal, 2026-11-08. Warm workers, interleaved calls,
unique inputs per repetition, correctness checked on every call.

| | |
|---|---|
| Machine | Intel Core Ultra 7 255U (12C/14T), 32 GB, Windows 11, power plan "High performance" |
| Rijo | Rijo Core `0.1.0` + the `--bench-stdio` harness in this repository — dispatch only, process warm |
| Hermes | `v0.22.2+2579.g601491f`, real `tools/*` functions, Python warm |
| Warmups | 2 per case/arm; measured 30 per case/arm/session |
| Sessions | 3, different times; inputs are 32 distinct files/patterns/commands |
| Correctness | marker/JSON check on every call |

## Per-session medians (µs)

| Case | Session | Rijo | Hermes | Ratio | Rijo ok | Hermes ok |
|---|---|---:|---:|---:|---:|---:|
| read_10kb | 194316 | 9072 | 775691 | 85.51× | 30/30 | 30/30 |
| read_10kb | 194651 | 4090 | 985704 | 241.00× | 30/30 | 30/30 |
| read_10kb | 194930 | 3742 | 887254 | 237.08× | 30/30 | 30/30 |
| search_literal | 194316 | 222940 | 1609066 | 7.22× | 30/30 | 30/30 |
| search_literal | 194651 | 247961 | 1575054 | 6.35× | 30/30 | 30/30 |
| search_literal | 194930 | 287629 | 1953476 | 6.79× | 30/30 | 30/30 |
| terminal_echo | 194316 | 89149 | 831410 | 9.33× | 30/30 | 30/30 |
| terminal_echo | 194651 | 63150 | 511502 | 8.10× | 30/30 | 30/30 |
| terminal_echo | 194930 | 71280 | 525990 | 7.38× | 30/30 | 30/30 |
| skills_list | 194316 | 8364 | 75128 | 8.98× | 30/30 | 30/30 |
| skills_list | 194651 | 7636 | 67430 | 8.83× | 30/30 | 30/30 |
| skills_list | 194930 | 2934 | 23756 | 8.10× | 30/30 | 30/30 |

## Pooled over all 90 repetitions

| Case | Arm | n ok | median µs | p5 µs | p95 µs | Ratio range (sessions) |
|---|---|---:|---:|---:|---:|---:|
| read_10kb | rijo | 90/90 | 4204 | 1981 | 21714 | 85.51–241.00× |
| read_10kb | hermes | 90/90 | 876814 | 611170 | 1523554 | — |
| search_literal | rijo | 90/90 | 253406 | 160853 | 1364685 | 6.35–7.22× |
| search_literal | hermes | 90/90 | 1621613 | 994149 | 6788398 | — |
| terminal_echo | rijo | 90/90 | 78718 | 39766 | 211256 | 7.38–9.33× |
| terminal_echo | hermes | 90/90 | 645816 | 398100 | 1491100 | — |
| skills_list | rijo | 90/90 | 7364 | 2505 | 15164 | 8.10–8.98× |
| skills_list | hermes | 90/90 | 61580 | 22821 | 125758 | — |

## Verdict

- **read_10kb — robust.** Rijo's median is 3.7–9.1 ms, Hermes' is 776–986 ms;
  the distributions do not overlap in any session (Rijo p95 ≤ 30 ms, Hermes p5 ≥ 581 ms).
  The factor is session-dependent (85–241×), so report the range, not one number.
- **search_literal — robust in 2 of 3 sessions on p5/p95, median direction in all 3.**
  Medians: 6.35–7.22×. Session 1 had outliers on both arms (Rijo max 4.7 s, Hermes max 13.3 s);
  sessions 2 and 3 do not overlap at p95/p5. The honest sentence is
  "Rijo is faster by ~6–7× in the median; the search case is I/O-noisy."
- **terminal_echo — robust.** 7.38–9.33×; Rijo's p95 stays below Hermes' p5 in all
  three sessions. This is process-spawn plus shell overhead, not arithmetic.
- **skills_list — indicative, not comparable.** 8.1–9.0×, but the two arms read
  different skill libraries (Rijo's own + foreign, Hermes' own). The timing is
  implementation-plus-library-size; do not put it in the headline until both arms
  read the same fixture skills directory.

## What this replaces

The old `188–1037×` numbers are **not** supported by this run. They compared a warm
Rijo call with single-shot Hermes calls (and in some cases a blocked repeat): the old
`terminal_echo_us` of 5.9 s is close to Hermes' cold first call, not its warm median.
The fair warm signal is roughly **6–9× on search/terminal** and **85–241× on a 10 KB
read**, with the search caveat above.

## What this is not

- One machine, one OS, one session series. A different laptop or a cold page cache
  changes absolute times.
- OpenCode is not in this run. The plan's OpenCode adapter is still a go/no-go.
- No end-to-end agent tasks, no model, no token cost. This is the tool layer only.
- The `skills_list` row is not a controlled comparison (different libraries).

## Raw data

- `bench/results/2026-11-08_194316/raw/micro_rijo_hermes.jsonl`
- `bench/results/2026-11-08_194651/raw/micro_rijo_hermes.jsonl`
- `bench/results/2026-11-08_194930/raw/micro_rijo_hermes.jsonl`

# Security and trust model

Rijo Code is an agent harness that runs shell commands, writes files and talks to a
model API. This document states plainly what the program does, what it does not
do, and which limits are deliberate.

## Where data stays

- **Local by default:** workspace files and `~/.rijo/` (config, sessions,
  skills, memory, traces). None of it is uploaded automatically.
- **Sent to the configured model provider:** the system prompt, the
  conversation, tool results and the skill/memory lists — that is the point of
  the program. The destination is `llm.base_url` in `~/.rijo/config.yaml`
  (OpenAI-compatible; a local endpoint works just as well). Change it if you
  want your traffic to go somewhere else.
- **Web tools:** `web_search` queries the search providers built into the binary
  (a keyless default is included — no key and no daemon required);
  `web_extract` fetches exactly the URLs you pass it; `browser_*` drives a
  local Chrome only when `browser_backend: cdp` is set. Nothing is uploaded
  beyond the search queries and the pages you asked for.
- **Secret-file protection:** `read_file`, `search_files`, `write_file`, `patch`,
  `delete_file` and `skill_view` refuse `.env` and secret variants (including
  symlink aliases); templates remain readable. Values loaded from `.env` live
  in a sealed store and are never put into the prompt. The sudo integration
  sends its password only over the child stdin pipe, never in shell text or argv.

## Approvals (guard)

- Recursive deletes (`rm -rf`), `mkfs`, `dd`, `git reset --hard`, `sudo` and
  delete operations in Python code (`shutil.rmtree`, `os.remove`, …) are
  intercepted **before** execution, on every execution path (`terminal`,
  `execute_code`, `delete_file`, including commands hidden inside scripts).
- Approval is only possible through a dialog in the TUI; headless runs are
  refused (fail-closed). Self-approval via environment variables smuggled into
  the payload is rejected outright.
- Every decision is written to the audit trail `~/.rijo/data/guard_trace.jsonl`.
- **Honest limit:** the guard is a text heuristic, **not a sandbox**. It catches
  the common shapes of destructive commands, but a determined detour
  (obfuscated shell invocations, new tooling) can get around it. If you need
  real isolation, use the sandbox below or run Rijo Code in a container under a
  dedicated user.

## Execution sandbox (ADR 0018)

`terminal` and `execute_code` honour `sandbox.backend`:

- `off` (default) — the process runs on the host, unchanged.
- `bwrap` — the command runs under **bubblewrap** (Linux): read-only system, the
  workspace writable, private `/tmp`/`/dev`/`/proc`, network optional
  (`sandbox.network`). Opt-in; on a machine without `bwrap` it fails closed
  instead of silently running on the host.
- `box` — the command runs inside a **Rijo Box** (`box.v1` tool socket). The
  client fails closed while no box is reachable.

`execute_code` has a per-call timeout (defaults to `terminal.timeout`, max 600 s),
kills its process tree on expiry, and caps each returned output stream at 50,000
bytes. `search_files` caps aggregate ripgrep output at 1 MB and stops the child
when that budget is exceeded. These are resource bounds, not filesystem
isolation.

The guard decides *whether* a command may run; the sandbox decides *what an
allowed process may touch*. `bwrap` is Linux-only — there is no equivalent that
restricts a child process's file access on Windows, so there only `off` (or
`box`) apply. Background execution is **refused** under a non-`off` backend
rather than silently downgraded.

## What the agent may do without asking

File edits inside the workspace, shell commands (other than the guarded ones),
starting and stopping processes, web fetches, and writing skills/memory — each
of those inside `~/.rijo/`. Configured external skill roots are read-only.

## Network access (overview)

| Purpose | Destination |
|---|---|
| Model calls | `llm.base_url` from the config |
| Web search | the search providers built into the binary |
| Web extract | exactly the URLs passed to `web_extract` |
| Browser tools | a local Chrome, only when `browser_backend: cdp` is set |
| Build time | crates.io; the `ort` crate downloads the ONNX runtime once |
| Embeddings | fastembed downloads its model into `~/.rijo/models/fastembed` (once; override with `RIJO_MODEL_CACHE`) |

## Reporting

Please report vulnerabilities **not** through public issues but via GitHub
Security Advisories or directly to the maintainers (Rijoda Labs). Include a
reproduction, the version (`rijo --version`) and your operating system.

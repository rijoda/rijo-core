# Installation

## Quick path (build from source)

```bash
git clone https://github.com/rijoda/rijo-core.git
cd rijo-core
./install.sh
```

The script builds the release binary and installs it to `~/.local/bin/rijo`
(no `sudo`). Then run `rijo` inside a project — the first start shows a short
onboarding (provider, base URL, model, API key) and writes
`~/.rijo/config.yaml` with mode 600.

Options:

```bash
./install.sh --prefix ~/bin        # different target directory
./install.sh --binary v0.1.0       # download a prebuilt release asset instead of building
./install.sh --check               # only verify the prerequisites
./install.sh --dry-run             # show what would happen
./install.sh --uninstall           # remove the binary (~/.rijo stays)
```

## Requirements

| What | Why | Check |
|---|---|---|
| Rust ≥ 1.88 (edition 2021, workspace `rust-version`) | build | `cargo --version` |
| git | cloning, repo status inside the agent | `git --version` |
| Network for the first build | crates.io; the `ort` crate downloads the ONNX runtime | — |
| Linux (tested: Arch/Hyprland) | terminal/process handling is POSIX-specific | `uname -s` |
| An API key for an OpenAI-compatible provider | model calls | — |
| Optional: Chrome/Chromium plus `browser_backend: cdp` | browser tools | — |

No Rust yet? Install it without `sudo`, inside your home:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

## Configuration (`~/.rijo/config.yaml`)

Onboarding creates the file. Minimal example:

```yaml
llm:
  base_url: https://api.deepseek.com/v1   # any OpenAI-compatible endpoint
  api_key: ""                             # never commit this
  model: deepseek-chat
  max_tokens: 4096
  temperature: 0.7
skills_dir: ~/.rijo/skills
data_dir: ~/.rijo/data
terminal_timeout: 180
learning:
  after_turn: true                        # learning review after each turn
  min_iterations: 2
  max_tokens: 800
```

**No file editing needed for a known provider.** `--provider deepseek` points at
`https://api.deepseek.com/v1`, takes the key from `DEEPSEEK_API_KEY` and defaults
to `deepseek-chat`. `--list-providers` shows the ready-made ones (DeepSeek,
OpenCode Zen/Go) with their endpoints and key variables. `--model` overrides the
default; a model you set in `config.yaml` is never replaced.

- `RIJO_API_KEY` as an environment variable overrides the key.
- `RIJO_SKIP_ONBOARD=1` skips onboarding (headless/scripts).
- `RIJO_SKIP_TRUST=1` starts without the workspace trust prompt (throwaway
  environments only).
- `--config <file>` uses a different config (useful for measurements, see
  `scripts/ab_learning.py`).
- `RIJO_RELEASE_URL_BASE` overrides the download base for `--binary` — useful
  for a mirror or for testing the install path against a local directory.

## Where things live

```
~/.rijo/config.yaml        config (600)
~/.rijo/data/              sessions (sessions.db), traces, metrics, process logs
~/.rijo/skills/            own and learned skills
~/.rijo/models/fastembed/  embedding model (downloaded once)
```

## First steps

```bash
rijo                      # interactive (legacy TUI) in the current directory
rijo --query "…"          # one-shot run, no TUI
rijo --resume             # continue the last session
rijo --tools              # list registered tools
rijo --bench              # measure tool latency (Rijo Code vs. Hermes reference)
```

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| Build fails on `ort` | `ort` is pinned to `2.0.0-rc.13`; `ort = "2"` does not exist. It needs `default-features = false, features = ["download-binaries"]` and network access during the build. |
| `cargo test` runs but reports no tests | Known artifact: a stale binary sits in `target/debug/deps/<crate>-<hash>`. Workaround: `touch crates/rijo-core/src/lib.rs && cargo test -p rijo-core --lib`. |
| First start takes long | fastembed downloads `bge-small-en-v1.5` into `~/.fastembed_cache` (once). |
| "BLOCKED: command looks dangerous" | That is the guard. Approval only through the interactive dialog; headless is refused. See `SECURITY.md`. |
| Starting a server hangs the turn | Use `terminal` with `background: true`, then check in a SEPARATE call (`curl -s -o /dev/null -w "%{http_code}" …`). |
| Browser tools find nothing | Set `browser_backend: cdp` and start Chrome with a debug port. |
| `install.sh --binary` refuses with "checksum mismatch" | The published `.sha256` does not match the archive. Re-download; if it persists, the release assets are broken — report it. |

## Uninstall

```bash
./install.sh --uninstall     # removes the binary only
rm -rf ~/.rijo              # config, sessions, skills, memory (permanent)
```

## For contributors

```bash
cargo check -p rijo-core            # fast compile check (preferred)
cargo test --workspace               # unit + integration tests
cargo build --release -p rijo        # release binary (LTO, see root Cargo.toml)
./install.sh                         # build + install, same path as above
```

CI (`.github/workflows/ci.yml`) builds and tests on every push/PR. A tag `v*`
triggers `.github/workflows/release.yml`, which attaches the binary plus its
SHA256 to the release.

> **Status 2026-11-08:** this public repository runs the CI workflow in
> `.github/workflows/ci.yml` (green on Linux and Windows). The private development
> repository had been blocked by a billing/spending limit; local `cargo test` is still the
> quickest signal.

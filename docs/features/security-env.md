## Security: Env Key Isolation

**File:** `src/trust.rs`  
**Status:** ✅ Implemented  
**Since:** Phase 3

### Goal

Prevent the LLM from reading API keys, database passwords, and other secrets
stored in `.env` files. Even if the agent is prompted to "read the .env file",
it cannot.

### How it works

1. **Startup injection** — When Rijo Code starts, `trust::load_env_file()` reads
   `{workspace}/.env`, parses `KEY=VALUE` pairs, and stores them in a
   process-global `SealedEnv` (a `Mutex<HashMap<String, String>>`).

2. **Tool blocking** — `read_file`, `search_files`, `write_file`, `patch`,
   `delete_file` and `skill_view` use `trust::is_env_file()`. It matches `.env`
   and secret variants, including symlink aliases; templates (`.env.example`,
   `.env.sample`, `.env.template`, `.env.dist`) stay readable. Search also
   disables user ripgrep config and filters its fallback walker.

3. **Recursive delete** — `delete_file` refuses an env file and refuses a tree
   containing one. Its preflight does not follow directory symlinks, matching
   `remove_dir_all` behavior and avoiding symlink cycles.

4. **Selective access** — Only the specific integrations that need a key call
   `trust::get_sealed("KEY")` (today: sudo). The sudo password is written to the
   child stdin pipe; it is never interpolated into shell text or command-line
   arguments. The LLM **never** receives the value.

5. **No general shell injection** — Sealed variables are not added to the
   `terminal` or background-process environment. Those are model-driven, so an
   injected key could be read straight back with `env` or `echo $KEY`. Only the
   sudo integration receives its one password via stdin.

### Security boundary

```
  User               Rijo Code                     LLM
   │                  │                         │
   │  .env            │  read_file(".env")      │
   │ ──────►          │  ──────────────────►    │
   │  (SealedEnv)     │  ← "Refused" ───────── │
   │                  │                         │
   │                  │  web_search()           │
   │                  │  ──────────────────►    │
   │                  │  (reads the key from    │
   │                  │   SealedEnv itself; the │
   │                  │   LLM never sees it)    │
```

### User interface

- At startup: `"  .env loaded: 3 variable(s) protected."`
- On tool error: the relevant file operation refuses `.env` access
- `env` commands (planned): `rijo env list`, `rijo env set KEY=VALUE`

### Implementation notes

- Uses `std::sync::LazyLock<Mutex<HashMap>>` — no external dependencies,
  thread-safe, lazy-initialised on first access.
- Paths are matched by filename — `.env` and `.env.*` variants — not full path,
  so any such file anywhere on the filesystem is protected. Templates
  (`.env.example`, …) are exempt so they can be read.
- `write_file` and `patch` refuse `.env` files; the model cannot create or edit
  a secret file through those tools either.

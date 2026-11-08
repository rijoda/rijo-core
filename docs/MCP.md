# MCP — server and client

`rijo mcp serve` runs Rijo Code as a **Model Context Protocol** server over **stdio**. Any MCP
client — Claude Code, Codex, Cursor, VS Code, Hermes — can then call Rijo's native tools.

## Start

```bash
rijo mcp serve
```

Client configuration (Claude Code / Codex / Cursor style):

```json
{ "mcpServers": { "rijo": { "command": "rijo", "args": ["mcp", "serve"] } } }
```

## Two tools, one registry

| Tool | Arguments | Returns |
|---|---|---|
| `list_tools` | — | `{ "count", "tools": [{ "name", "description", "toolset", "async", "input_schema" }] }` |
| `call_tool` | `name`, `arguments` (object) | the tool's result as text |

Call `list_tools` once, then `call_tool`. The registry is the single source of truth, so the MCP
surface cannot drift from the native one. Names are sorted (`Registry::get_names`) — the same
determinism the prompt cache relies on.

## The 34 native tools

| Toolset | Tools |
|---|---|
| file | `read_file`, `write_file`, `patch`, `search_files`, `delete_file` |
| terminal | `terminal`, `process_manage` |
| git | `git` |
| web | `web_search`, `web_extract` |
| browser | `browser_navigate`, `browser_snapshot`, `browser_click`, `browser_type`, `browser_scroll`, `browser_dev` |
| research | `arxiv` |
| brain | `brain_search`, `brain_outline` |
| skills | `skills_list`, `skill_view`, `skill_manage` |
| github | `github` |
| integration | `mcp` (consumes *external* MCP servers — see below) |
| core | `clarify`, `context`, `cronjob_manage`, `delegate_task`, `execute_code`, `memory`, `session_search`, `text_to_speech`, `todo_list`, `vision_analyze` |

## Safety — the Guard applies unchanged

`call_tool` dispatches through `Registry::dispatch`, so each tool's own safety checks run exactly as
in every other frontend. The server is **headless** (no confirmation channel), so the Guard **denies dangerous
operations by default**:

```json
{ "error": "BLOCKED (recursive delete): this terminal action needs the user's confirmation and there is no interactive channel in this run, so it is denied by default. Ask the user." }
```

Only `RIJO_APPROVE_DANGEROUS=1` in the server process opens that door. Every decision is written to
`~/.rijo/data/guard_trace.jsonl`.

## Protocol hygiene

With stdio MCP, stdout **is** the JSON-RPC channel. Nothing in the server may print to stdout;
diagnostics go to stderr. No tool prints to stdout, and a stray `println!` would corrupt the stream.

## Client — consume external servers (ADR 0017)

Rijo Code is not only a server: the native `mcp` tool consumes **external** MCP
servers over **stdio**. Configure them under `mcp:` in `~/.rijo/config.yaml`:

```yaml
mcp:
  servers:
    - name: filesystem
      command: npx
      args: ["-y", "@modelcontextprotocol/server-filesystem", "/some/dir"]
      read_only_tools: [read_file, list_directory]   # run without a dialog
```

The agent sees three actions on one stable tool — the schema never changes, so
the alphabetical tool order and the prompt cache stay intact:

| Action | Arguments | Returns |
|---|---|---|
| `list_servers` | — | the configured servers (no connection) |
| `list_tools` | `server` | that server's tools with their JSON schemas |
| `call` | `server`, `tool`, `arguments` | the remote tool's result as text |

**Guard policy.** A remote tool has no native guard of its own, so a call runs
without asking **only** when its name is in the server's `read_only_tools`.
Every other call goes through the same guard as `terminal`/`execute_code`: TUI
confirmation, denied by default in headless runs (`RIJO_APPROVE_DANGEROUS=1`
pre-approves for that process). An undeclared tool is denied *before* a process
is spawned. `mcp` is not in the parallel-batch allowlist, so its calls stay
strictly ordered.

Inspect the configuration without a model turn:

```bash
rijo mcp list            # the configured servers
rijo mcp list <server>   # that server's tools (opens one connection)
```

Connections are opened per call; pooling and reconnect are open (below). Server
startup/initialize is bounded to 45 seconds. Each `tools/list` and `tools/call`
request is bounded to 60 seconds; a nonresponsive server returns a named timeout
error instead of holding the turn forever.

### Verified (2026-10-07)

Against Rijo Code's own `rijo mcp serve` (dogfooding, offline):

- `crates/rijo-cli/tests/mcp_client_contracts.rs` — connect, `list_tools`,
  `call`: **2 green**
- `crates/rijo-cli/tests/mcp_tool_contracts.rs` — the tool through the real
  registry: `list_servers` (no connection), `list_tools` (schemas), an
  undeclared remote tool **denied headless**, a declared read-only tool runs
  without a dialog: **4 green**

### Open

- Pooling/reconnect of server processes (per-call spawn today).
- Streamable-HTTP transport and auth for remote servers.
- Named namespaced tools (`mcp__<server>__<tool>`) instead of the proxy action —
  only if a measurement shows the extra discovery hop costs real iterations.
- Prompts and resources (tools only for now).

## Verified (2026-10-01)

Real stdio handshake (`initialize` → `tools/list` → `tools/call`):

- `initialize` returns name `rijo`, version `0.1.0`, `capabilities.tools`
- `list_tools` returns **29** tools with their JSON schemas
- `call_tool` → `read_file` returns file content
- `call_tool` → `terminal {"command":"rm -rf build"}` returns `BLOCKED (recursive delete)`, and the
  guard trail records `denied-headless`
- stdout carried exactly the protocol responses and nothing else

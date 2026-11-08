# Changelog

> **Historical.** This was the changelog up to the 2026-09-06 phase. The canonical
> changelog is the repository-root [`CHANGELOG.md`](../../CHANGELOG.md) (Keep a
> Changelog, current release `0.1.0`). This file is kept as the early-phase record
> and is no longer updated.

## Phase 3 — Subsecond + Security (2026-09-06)

### Added
- **`delete_file` tool**: Workspace-scoped, per-delete user confirmation,
  `.env` protection
- **`trust` module**: First-run workspace trust prompt + marker system
- **`.env` key isolation**: Sealed arena prevents LLM from reading secrets;
  `read_file` and `delete_file` block `.env` access
- **`docs/` directory**: Feature documentation per module
- **`CHANGELOG.md`**: This file

### Changed
- **TUI entry rendering**: flame-icon header (`──  rijo `) once per
  assistant reply, no per-line labels. Separator lines between entries.
  Tool calls as a compact `◌ tool` / `✓ tool · 1.2s` box.
- **Scroll system**: `CACHED_MAX_SCROLL` cache, anchor clipping for
  consistent scrolling even after resize/entry deletion
- **Skills Watcher**: mtime snapshot comparison instead of always firing
- **Binary deploy**: `~/.local/bin/rijo`

### Fixed
- Duplicate assistant replies (StreamDone + entry conflict deduplicated)

## Phase 2 — Hot-Reload Binary Self-Swap (2026-09-05)

### Added
- `selfupd` module: staged binary swap via `~/.rijo/staging/rijo.new`
- `update` command: `/update` in the TUI + `RIJO_AUTO_SELFUPDATE=1`
- Graceful restart with `--resume` marker
- Watcher for staging directory

## Phase 1 — Hot-Reload Config + Skills (2026-09-05)

### Added
- `watcher` module: notify-based FS watcher
- Config reload on `config.yaml` change → `TuiEvent::ConfigReloaded`
- Skills reload on `skills/` change → `TuiEvent::SkillsReloaded`
- 800ms debounce to absorb editor save bursts

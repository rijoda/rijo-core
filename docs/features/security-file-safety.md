## Security: File Safety

**File:** `src/tools/file_tools.rs`, `src/trust.rs`, `src/tools/skills_tool.rs`
**Status:** ✅ Implemented  
**Since:** Phase 3 (2026-09-06)

### Scope restrictions

`write_file`, `patch` and `delete_file` only mutate files **inside the project
workspace**. The workspace is determined by `config.workspace` which defaults
to the current directory at startup. Tilde paths are expanded before checking;
their original spelling is not an authorization exception.

```
trust::is_in_workspace(&path, &state.config.workspace)
```

Path checks resolve existing ancestors too, so a new file below
`workspace/link -> outside` is rejected even though the final file does not
exist yet. `patch` checks the workspace independently of read-before-edit.

`skill_manage` writes, patches and deletes only in Rijo's own `skills_dir`;
configured foreign skill roots are read-only. Skill names and categories are
single path components, and linked skill files must resolve beneath the skill
directory.

### Per-delete confirmation

Every delete requires explicit user confirmation:

1. If `RIJO_APPROVE_DANGEROUS=1` is set → auto-approved
2. Otherwise → `cli::prompt_dangerous()` prints:
   ```
   ⚠  DELETE /path/to/file? This file will be deleted permanently.
      Confirm with 'y' / N to abort: _
   ```
3. If the user answers anything other than `y`/`yes`/`j`/`ja` → aborted

### .env protection

`read_file`, `search_files`, `write_file`, `patch`, `delete_file` and `skill_view`
refuse `.env` and secret variants, including symlink aliases. Recursive search
ignores ripgrep user config, and its fallback walker filters env files and does
not follow symlinks.

### Related

- [Env Key Isolation](security-env.md)
- [Trust System](security-trust.md)

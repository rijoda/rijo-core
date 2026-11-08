## Security: Trust System

**File:** `src/trust.rs`  
**Status:** ✅ Implemented  
**Since:** Phase 3 (2026-09-06)

### Goal

On first run in an unknown workspace directory, prompt the user to confirm
it is safe before Rijo Code takes destructive actions.

### How it works

1. At startup, `trust::is_trusted(&config.workspace)` derives a stable marker
   name from the canonical workspace path and checks that the marker file
   contains that exact path. Distinct paths cannot trust each other through
   separator-flattening collisions.

2. If no marker exists, `trust::prompt_trust()` asks via stderr:
   ```
   ⚠  Rijo Code was started in a new directory:
      /home/user/projects/my-project

      Mark this directory as trusted?
      Rijo Code can then read, write and delete files here.
      [y/N]
   ```

3. If the user declines, Rijo Code exits with status 0.

4. If accepted, the canonical workspace path is written to its marker. The
   content check prevents even a hash collision from trusting the wrong path.

### Bypass

Set `RIJO_SKIP_TRUST=1` to skip the trust prompt.

### Trust marker storage

```
~/.rijo/trusted/
  <stable-path-hash>.trust   # contains the canonical workspace path
```

One marker per workspace, validated against the canonical path. Old flattened
empty markers are intentionally not trusted because collisions cannot be
disambiguated; after upgrading, each workspace prompts once again.

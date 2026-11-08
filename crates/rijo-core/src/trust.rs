//! trust — project-directory trust prompt + env key isolation
//!
//! On first run in an unknown workspace, prompts the user to confirm the
//! directory is safe. Once trusted, stores a marker in ~/.rijo/trusted/.
//!
//! Env key isolation: provides a sealed arena that the LLM-brained tool
//! executor cannot read — keys live behind a `SealedEnv`. They are read only by
//! the specific integrations that need them (today: the sudo-password
//! injection) and are **never** passed to the LLM prompt, to `read_file`, or to
//! the general shell.
//!
//! Deliberately NOT injected into `terminal`/`process` commands: the model
//! drives those, so any injected key could be read straight back with `env` or
//! `echo $KEY`. Sealing only helps while the shell cannot see the keys.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

// ─── Trust ────────────────────────────────────────────────────────────────

const TRUST_DIR: &str = "trusted";

/// Returns a collision-resistant, bounded marker path for a workspace.
fn trust_marker(workspace: &Path) -> PathBuf {
    let canonical = canonical_workspace(workspace);
    // The marker contents are checked against `canonical_workspace` too, so a
    // hash collision can at worst cause a repeat prompt, never cross-trust.
    let key = stable_path_key(&canonical);
    crate::paths::rijo_home()
        .join(TRUST_DIR)
        .join(format!("{key}.trust"))
}

fn stable_path_key(path: &str) -> String {
    // FNV-1a is not used as a security primitive here; it is a stable filename
    // key. The marker content is compared to the full canonical path, so a
    // collision fails closed and only causes a repeat trust prompt.
    let mut hash = 0xcbf29ce484222325u64;
    for byte in path.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn canonical_workspace(workspace: &Path) -> String {
    workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf())
        .to_string_lossy()
        .to_string()
}

/// Check whether a workspace has been marked trusted.
pub fn is_trusted(workspace: &Path) -> bool {
    let canonical = canonical_workspace(workspace);
    std::fs::read_to_string(trust_marker(workspace))
        .map(|stored| stored == canonical)
        .unwrap_or(false)
}

/// Mark a workspace as trusted (stores the canonical workspace path).
pub fn mark_trusted(workspace: &Path) -> Result<(), String> {
    let marker = trust_marker(workspace);
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create trust dir: {e}"))?;
    }
    std::fs::write(&marker, canonical_workspace(workspace))
        .map_err(|e| format!("cannot write trust marker: {e}"))
}

/// Prompt the user via TUI to accept or reject a workspace as trusted.
/// Returns true if the user accepted.
pub fn prompt_trust(workspace: &Path) -> bool {
    eprintln!("\n⚠  Rijo Code was started in a new directory:");
    eprintln!("   {}", workspace.display());
    eprintln!("\n   Mark this directory as trusted?");
    eprintln!("   Rijo Code can then read, write and delete files here.");
    eprint!("   [y/N] ");
    use std::io::Write;
    let _ = std::io::stderr().flush();
    let mut input = String::new();
    match std::io::stdin().read_line(&mut input) {
        Ok(_) => {
            let trimmed = input.trim().to_lowercase();
            trimmed == "j" || trimmed == "ja" || trimmed == "y" || trimmed == "yes"
        }
        Err(_) => false,
    }
}

// ─── Sealed env: keys that the LLM cannot read ───────────────────────────

/// Sealed environment: workspace `.env` keys, readable only by the specific
/// integrations that call [`get_sealed`] (today: the sudo password). They are
/// **never** handed to the LLM prompt nor injected into the general shell — a
/// shell is model-driven, so injection would leak them. Process-global store.
static SEALED_ENV: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Seed the sealed env from a .env file in the workspace.
/// Reads the file, strips it, stores keys in the sealed arena.
/// Returns the list of variable names loaded (without values).
pub fn load_env_file(workspace: &Path) -> Result<Vec<String>, String> {
    let env_path = workspace.join(".env");
    if !env_path.exists() {
        return Ok(Vec::new());
    }
    let content =
        std::fs::read_to_string(&env_path).map_err(|e| format!("cannot read .env: {e}"))?;

    let mut map = SEALED_ENV.lock().unwrap();
    let mut names = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(eq_pos) = trimmed.find('=') {
            let key = trimmed[..eq_pos].trim().to_string();
            let value = trimmed[eq_pos + 1..].trim().to_string();
            let value = if (value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\''))
            {
                value[1..value.len() - 1].to_string()
            } else {
                value
            };
            map.insert(key.clone(), value);
            names.push(key);
        }
    }
    Ok(names)
}

/// Get a sealed env value (for tools that need it like terminal).
pub fn get_sealed(key: &str) -> Option<String> {
    let map = SEALED_ENV.lock().unwrap();
    map.get(key).cloned()
}

/// Set a sealed env value from within Rijo Code (the `env` command).
pub fn set_sealed(key: String, value: String) {
    let mut map = SEALED_ENV.lock().unwrap();
    map.insert(key, value);
}

/// List all sealed env variable names (no values).
pub fn list_sealed() -> Vec<String> {
    let map = SEALED_ENV.lock().unwrap();
    let mut keys: Vec<String> = map.keys().cloned().collect();
    keys.sort();
    keys
}

/// Check if a path is inside the workspace directory.
///
/// Relative paths are resolved against the workspace (not the process cwd) and
/// the comparison is lexically normalized first — `Path::canonicalize()` only
/// works on paths that already exist, so canonicalizing alone would REJECT
/// every write of a new file (`e2e_webapp/index.html` → fallback to a relative
/// path → never a prefix of the absolute workspace).
pub fn is_in_workspace(path: &Path, workspace: &Path) -> bool {
    let Some(ws_abs) = comparable(&absolutize(workspace)) else {
        return false;
    };
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        ws_abs.join(path)
    };
    // Resolve the nearest existing ancestor even when the final file does not
    // exist yet. Otherwise `workspace/link/new.txt` escapes through `link ->
    // outside` because canonicalize(new.txt) fails and the old code compared a
    // purely lexical path.
    let Some(resolved) = comparable(&candidate) else {
        return false;
    };
    path_starts_with(&resolved, &ws_abs)
}

/// A form of `path` that can be compared against another such form.
///
/// Absolute, `.`/`..` resolved, symlinks followed when the path exists — and on
/// Windows **without** the `\\?\` verbatim prefix that `canonicalize()` adds.
/// Without that step a canonicalized file path (`\\?\C:\ws\f.txt`) would never
/// prefix-match a lexically built workspace path (`C:\ws`), because
/// `Path::starts_with` compares components and `\\?\C:` is not `C:`. On Linux
/// both forms coincide, which is why the check worked there and rejected every
/// write here.
fn comparable(path: &Path) -> Option<std::path::PathBuf> {
    use std::ffi::OsString;

    let normalized = normalize_lexical(&absolutize(path));
    let mut probe = normalized.clone();
    let mut missing: Vec<OsString> = Vec::new();
    loop {
        match std::fs::symlink_metadata(&probe) {
            Ok(_) => {
                // If an existing path cannot be canonicalized (notably a
                // dangling symlink), fail closed. Treating it as an ordinary
                // missing component could let create_dir_all follow it outside.
                let mut resolved = probe.canonicalize().ok()?;
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Some(strip_verbatim(&normalize_lexical(&resolved)));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
        let name = probe.file_name().map(OsString::from)?;
        missing.push(name);
        if !probe.pop() {
            return None;
        }
    }
}

/// Removes the `\\?\` (or `\\?\UNC\`) prefix Windows adds to canonical paths.
fn strip_verbatim(path: &Path) -> std::path::PathBuf {
    let Some(s) = path.to_str() else {
        return path.to_path_buf();
    };
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return std::path::PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return std::path::PathBuf::from(rest);
    }
    path.to_path_buf()
}

/// `Path::starts_with`, but case-insensitive on Windows — the platform treats
/// `C:\Users` and `c:\users` as the same directory, and rejecting a write over
/// letter case would be a bug, not a protection.
fn path_starts_with(path: &Path, base: &Path) -> bool {
    let mut rest = path.components();
    for want in base.components() {
        match rest.next() {
            Some(got) if component_eq(&got, &want) => {}
            _ => return false,
        }
    }
    true
}

fn component_eq(a: &std::path::Component, b: &std::path::Component) -> bool {
    if a == b {
        return true;
    }
    if cfg!(windows) {
        let (a, b) = (
            a.as_os_str().to_string_lossy(),
            b.as_os_str().to_string_lossy(),
        );
        return a.eq_ignore_ascii_case(&b);
    }
    false
}

/// Make a path absolute against the current working directory.
fn absolutize(path: &Path) -> std::path::PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(path)
    }
}

/// Resolve `.` / `..` without touching the filesystem, so traversal
/// (`workspace/../../etc/passwd`) cannot sneak past the prefix check.
fn normalize_lexical(path: &Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Check if a path is a secret env file.
///
/// Matches `.env` and its variants (`.env.local`, `.env.production`, …) — all
/// carry secrets. Templates (`.env.example`, `.env.sample`, `.env.template`,
/// `.env.dist`) are deliberately **not** matched: they are meant to be read and
/// hold no real keys.
pub fn is_env_file(path: &Path) -> bool {
    fn is_env_name(path: &Path) -> bool {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        if name == ".env" {
            return true;
        }
        match name.strip_prefix(".env.") {
            Some(variant) => !matches!(variant, "example" | "sample" | "template" | "dist"),
            None => false,
        }
    }

    is_env_name(path)
        || path
            .canonicalize()
            .map(|resolved| is_env_name(&resolved))
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_ws(tag: &str) -> std::path::PathBuf {
        let ws = std::env::temp_dir().join(format!("rijo-trust-test-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&ws);
        ws
    }

    #[test]
    fn new_relative_file_is_allowed() {
        // Regression: canonicalize() fails on not-yet-existing files, which
        // used to reject every write of a new file given as a relative path.
        let ws = temp_ws("newrel");
        assert!(is_in_workspace(Path::new("sub/new_file.txt"), &ws));
    }

    #[test]
    fn existing_file_is_allowed() {
        let ws = temp_ws("exists");
        let ws = ws.canonicalize().unwrap_or(ws);
        let f = ws.join("exists.txt");
        std::fs::write(&f, "x").unwrap();
        assert!(is_in_workspace(&f, &ws));
        assert!(is_in_workspace(Path::new("exists.txt"), &ws));
    }

    #[test]
    fn relative_traversal_is_rejected() {
        let ws = temp_ws("trav");
        assert!(!is_in_workspace(Path::new("../outside.txt"), &ws));
        assert!(!is_in_workspace(Path::new("a/../../outside.txt"), &ws));
    }

    #[cfg(unix)]
    #[test]
    fn nonexistent_target_below_a_workspace_symlink_cannot_escape() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, workspace.join("link")).unwrap();

        assert!(
            !is_in_workspace(&workspace.join("link/new.txt"), &workspace),
            "a not-yet-created file under a symlink to outside must be rejected"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dangling_workspace_symlink_is_not_treated_as_a_safe_missing_parent() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        symlink(
            tmp.path().join("not-created-outside"),
            workspace.join("dangling"),
        )
        .unwrap();

        assert!(
            !is_in_workspace(&workspace.join("dangling/new.txt"), &workspace),
            "a dangling symlink could be followed by create_dir_all outside the workspace"
        );
    }

    #[cfg(windows)]
    #[test]
    fn nonexistent_target_below_a_workspace_junction_cannot_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let junction = workspace.join("junction");
        let status = std::process::Command::new("cmd.exe")
            .args([
                "/D",
                "/C",
                "mklink",
                "/J",
                junction.to_str().unwrap(),
                outside.to_str().unwrap(),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("cmd.exe");
        assert!(status.success(), "could not create junction for test");

        assert!(
            !is_in_workspace(&junction.join("new.txt"), &workspace),
            "a not-yet-created file under a junction to outside must be rejected"
        );
    }

    #[test]
    fn trust_markers_do_not_collide_when_path_separators_are_flattened() {
        let tmp = tempfile::tempdir().unwrap();
        let flat = tmp.path().join("foo_bar");
        let nested = tmp.path().join("foo").join("bar");
        std::fs::create_dir_all(&flat).unwrap();
        std::fs::create_dir_all(&nested).unwrap();

        assert_ne!(
            trust_marker(&flat),
            trust_marker(&nested),
            "distinct workspaces must not share a trust marker"
        );
    }

    #[test]
    fn absolute_outside_is_rejected() {
        let ws = temp_ws("abs");
        let outside = if cfg!(windows) {
            "C:\\Windows\\System32"
        } else {
            "/etc/passwd"
        };
        assert!(!is_in_workspace(Path::new(outside), &ws));
    }

    #[test]
    fn relative_workspace_resolves_against_cwd() {
        // config.workspace is "." in practice — the guard must still work.
        assert!(is_in_workspace(
            Path::new("nested/dir/file.txt"),
            Path::new(".")
        ));
    }

    #[test]
    fn secret_env_files_are_recognised_and_templates_are_not() {
        for secret in [".env", ".env.local", ".env.production", ".env.test"] {
            assert!(is_env_file(Path::new(secret)), "{secret} must be protected");
            assert!(is_env_file(Path::new(&format!("sub/{secret}"))));
        }
        for template in [".env.example", ".env.sample", ".env.template", ".env.dist"] {
            assert!(
                !is_env_file(Path::new(template)),
                "{template} is a template and must stay readable"
            );
        }
        assert!(!is_env_file(Path::new("environment.rs")));
    }

    #[cfg(unix)]
    #[test]
    fn env_alias_symlinks_are_still_recognised_as_secrets() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let secret = tmp.path().join(".env");
        let alias = tmp.path().join("notes.txt");
        std::fs::write(&secret, "SECRET=value").unwrap();
        symlink(&secret, &alias).unwrap();
        assert!(is_env_file(&alias));
    }
}

//! Rust build-acceleration hints — speed up cargo builds transparently.
//!
//! Research-backed levers (2026): mold/lld linker, cranelift debug backend,
//! cargo-nextest parallel runner, profile.dev opt-level + debug=1.
//! Linking is typically 50-80% of incremental build time — the single biggest win.

use std::path::PathBuf;

/// Detects `.cargo/config.toml`-linked faster linker presence — checks both
/// project-local `.cargo/config.toml` AND the global `~/.cargo/config.toml`.
fn has_fast_linker_config(workdir: &std::path::Path) -> bool {
    let candidates = [
        workdir.join(".cargo").join("config.toml"),
        dirs::home_dir()
            .map(|h| h.join(".cargo").join("config.toml"))
            .unwrap_or_default(),
    ];
    for p in &candidates {
        if p.exists() {
            if let Ok(s) = std::fs::read_to_string(p) {
                if s.contains("mold") || s.contains("lld") {
                    return true;
                }
            }
        }
    }
    false
}

/// Detect mold / lld binary presence on PATH (needed for the config to work).
fn linker_bin_available() -> bool {
    let path = std::env::var("PATH").unwrap_or_default();
    for bin in ["mold", "ld.lld"] {
        for dir in path.split(':') {
            if std::path::PathBuf::from(dir).join(bin).exists() {
                return true;
            }
        }
    }
    false
}

fn nextest_available() -> bool {
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in path.split(':') {
        if std::path::PathBuf::from(dir).join("cargo-nextest").exists() {
            return true;
        }
    }
    false
}

/// Returns an actionable hint when the workspace would benefit from build tuning.
/// Only fires on SLOW cargo calls (>20s) so it never produces noise.
pub fn build_hint(command: &str, workdir: Option<&str>, duration_ms: u64) -> Option<String> {
    let is_build = command.contains("cargo build")
        || command.contains("cargo check")
        || command.contains("cargo test")
        || command.contains("cargo clippy")
        || command.contains("cargo bench");
    if !is_build || duration_ms < 20_000 {
        return None;
    }
    let cwd: PathBuf = workdir
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    if has_fast_linker_config(&cwd) && linker_bin_available() {
        // Already has fast linker — but first builds with many deps still
        // spend most time COMPILING, not linking. Suggest opt-level tuning.
        let mut h = String::from(
            "hint: this cargo-call took >20s even with mold. \
             Compilation (not linking) is the bottleneck for dependency-heavy builds.\n",
        );
        h.push_str("  In Cargo.toml, add for dev builds (cuts ~1/2 of first-build time):\n");
        h.push_str("     [profile.dev.package.\"*\"]\n");
        h.push_str("     opt-level = 3\n");
        h.push_str("     [profile.dev]\n");
        h.push_str("     debug = 1\n");
        if !nextest_available() {
            h.push_str(&format!(
                "  {}) cargo install cargo-nextest --locked  # ~3x faster test runs\n",
                if linker_bin_available() { 2 } else { 3 }
            ));
        }
        return Some(h);
    }
    if !linker_bin_available() {
        // mold/lld not installed → the hint should still fire, just without the linker step
        // (the build was >20s, the user wants to know why)
    }

    let mut h = String::from(
        "hint: this cargo-call took >20s. Linking is typically 50-80% of incremental build time. Fast setup:\n",
    );
    if linker_bin_available() {
        h.push_str("  1) create .cargo/config.toml:\n");
        h.push_str("     [target.x86_64-unknown-linux-gnu]\n");
        h.push_str("     linker = \"clang\"\n");
        h.push_str("     rustflags = [\"-C\", \"link-arg=-fuse-ld=mold\"]\n");
    } else {
        h.push_str("  1) sudo pacman -S mold\n");
        h.push_str("  2) create .cargo/config.toml (see above)\n");
    }
    if !nextest_available() {
        h.push_str(&format!(
            "  {}) cargo install cargo-nextest --locked  # ~3x faster test runs\n",
            if linker_bin_available() { 2 } else { 3 }
        ));
    }
    h.push_str(&format!(
        "  {}) in Cargo.toml: [profile.dev.package.\"*\"] opt-level = 3; [profile.dev] debug = 1\n",
        if linker_bin_available() {
            if nextest_available() {
                2
            } else {
                3
            }
        } else {
            if nextest_available() {
                3
            } else {
                4
            }
        }
    ));
    Some(h)
}

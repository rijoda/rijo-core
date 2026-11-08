//! Tech-Stack Auto-Detection Engine
//!
//! Detects the primary programming language & toolchain of a workspace at
//! session start and returns a concise prompt block (~80-150 tokens).
//!
//! Design: pure file-existence checks (<5ms), no config parsing, no LLM.
//! The output is injected as a stable section in the system prompt, so it
//! benefits from prompt caching and survives context compression.

use std::path::Path;

/// Detected tech stack with toolchain details.
#[derive(Debug, Clone)]
pub enum DetectedStack {
    Rust {
        edition: String,
        has_workspace: bool,
    },
    Python {
        tool: PythonTool,
    },
    Node {
        pkg: PkgManager,
    },
    Go,
    Zig,
    Cpp {
        build: CppBuild,
    },
    Unknown,
}

#[derive(Debug, Clone)]
pub enum PythonTool {
    UvPip,
    Poetry,
    Other,
}

#[derive(Debug, Clone)]
pub enum PkgManager {
    Npm,
    Yarn,
    Pnpm,
    Bun,
}

#[derive(Debug, Clone)]
pub enum CppBuild {
    Make,
    Cmake,
    Meson,
}

/// Scan workspace directory for known marker files — fast, no parsing.
pub fn detect(workspace: &Path) -> DetectedStack {
    if !workspace.is_dir() {
        return DetectedStack::Unknown;
    }

    if workspace.join("Cargo.toml").exists() {
        let edition = read_toml_field(workspace.join("Cargo.toml"), "edition")
            .unwrap_or_else(|| "2021".into());
        let has_workspace = std::fs::read_to_string(workspace.join("Cargo.toml"))
            .ok()
            .map(|s| s.contains("[workspace]"))
            .unwrap_or(false);
        return DetectedStack::Rust {
            edition,
            has_workspace,
        };
    }

    if workspace.join("pyproject.toml").exists()
        || workspace.join("setup.py").exists()
        || workspace.join("requirements.txt").exists()
    {
        let tool = if workspace.join("pyproject.toml").exists() {
            let content =
                std::fs::read_to_string(workspace.join("pyproject.toml")).unwrap_or_default();
            if content.contains("[tool.poetry]") {
                PythonTool::Poetry
            } else {
                PythonTool::UvPip
            }
        } else {
            PythonTool::Other
        };
        return DetectedStack::Python { tool };
    }

    if workspace.join("package.json").exists() {
        let pkg = if workspace.join("bun.lock").exists() || workspace.join("bun.lockb").exists() {
            PkgManager::Bun
        } else if workspace.join("pnpm-lock.yaml").exists() {
            PkgManager::Pnpm
        } else if workspace.join("yarn.lock").exists() {
            PkgManager::Yarn
        } else {
            PkgManager::Npm
        };
        return DetectedStack::Node { pkg };
    }

    if workspace.join("go.mod").exists() {
        return DetectedStack::Go;
    }

    if workspace.join("build.zig").exists() || workspace.join("build.zig.zon").exists() {
        return DetectedStack::Zig;
    }

    if workspace.join("CMakeLists.txt").exists() {
        return DetectedStack::Cpp {
            build: CppBuild::Cmake,
        };
    }
    if workspace.join("meson.build").exists() {
        return DetectedStack::Cpp {
            build: CppBuild::Meson,
        };
    }
    if workspace.join("Makefile").exists() {
        return DetectedStack::Cpp {
            build: CppBuild::Make,
        };
    }

    DetectedStack::Unknown
}

/// Render the detected stack into a compact prompt block.
pub fn render_prompt(stack: &DetectedStack) -> String {
    match stack {
        DetectedStack::Rust {
            edition,
            has_workspace,
        } => {
            let ws = if *has_workspace { " (workspace)" } else { "" };
            format!(
                "## Tech Stack\n\
                 Language: Rust{ws}\n\
                 Edition: {edition} | Linker: mold (if installed) or rust-lld (default since 1.90)\n\
                 Build: cargo build --release | cargo check (verify)\n\
                 Test: cargo test | cargo nextest (if installed, ~3x faster)\n\
                 Lint: cargo clippy -- -D warnings\n\
                 Format: rustfmt\n\
                 Profile tuning: add [profile.dev.package.\"*\"] opt-level = 3 in Cargo.toml\n\
                 Idioms: prefer Result over unwrap(); use thiserror/anyhow for errors;\n\
                 use clap for CLIs; prefer workspace inheritance for shared deps."
            )
        }
        DetectedStack::Python { tool } => match tool {
            PythonTool::UvPip => "## Tech Stack\n\
                     Language: Python (uv — 10-100x faster than pip)\n\
                     Package: uv | Lock: uv.lock\n\
                     Test: uv run pytest\n\
                     Lint: uv run ruff check --fix .\n\
                     Format: uv run ruff format .\n\
                     Type check: uv run pyright . (or: ty, mypy)\n\
                     Upgrade syntax: uv run ruff check --select UP --fix\n\
                     Idioms: dataclasses over dicts; pathlib over os.path;\n\
                     type hints required; Pydantic v2 for validation;\n\
                     pip-audit for dependency scanning."
                .into(),
            PythonTool::Poetry => "## Tech Stack\n\
                     Language: Python\n\
                     Package: poetry\n\
                     Test: poetry run pytest\n\
                     Lint: poetry run ruff check --fix .\n\
                     Format: poetry run ruff format .\n\
                     Type check: poetry run mypy . (or pyright)\n\
                     Idioms: dataclasses over dicts; pathlib over os.path;\n\
                     type hints required; Pydantic v2 for validation."
                .into(),
            PythonTool::Other => "## Tech Stack\n\
                     Language: Python\n\
                     Test: python -m pytest\n\
                     Lint: ruff check --fix .\n\
                     Format: ruff format .\n\
                     Type check: mypy . (or pyright)\n\
                     Idioms: dataclasses over dicts; pathlib over os.path;\n\
                     type hints recommended."
                .into(),
        },
        DetectedStack::Node { pkg } => {
            let (run, test, lint, type_check) = match pkg {
                PkgManager::Bun => ("bun run", "bun test", "bun lint", "tsc --noEmit"),
                PkgManager::Pnpm => ("pnpm", "pnpm test", "pnpm lint", "tsc --noEmit"),
                PkgManager::Yarn => ("yarn", "yarn test", "yarn lint", "tsc --noEmit"),
                PkgManager::Npm => ("npx", "npm test", "npx biome check .", "tsc --noEmit"),
            };
            format!(
                "## Tech Stack\n\
                 Language: JavaScript/TypeScript\n\
                 Package: {}\n\
                 Build: {run} build\n\
                 Test: {test}\n\
                 Lint: {lint} | Format: biome check --write .\n\
                 Type check: {type_check}\n\
                 Idioms: prefer modern ESM over CJS; TypeScript strict mode required;\n\
                 use Biome over eslint+prettier; use vitest over jest;\n\
                 prefer Zod for validation; use Drizzle ORM for databases.",
                match pkg {
                    PkgManager::Bun => "bun",
                    PkgManager::Pnpm => "pnpm (with Bun as alternative)",
                    PkgManager::Yarn => "yarn (consider migrating to pnpm)",
                    PkgManager::Npm => "npm (consider migrating to pnpm)",
                }
            )
        }
        DetectedStack::Go => "## Tech Stack\n\
             Language: Go\n\
             Build: go build ./...\n\
             Test: go test ./...\n\
             Lint: go vet ./... | golangci-lint run\n\
             Format: go fmt\n\
             Idioms: prefer errors over panics; use go mod for deps;\n\
             idiomatic Go over framework-heavy patterns."
            .into(),
        DetectedStack::Zig => "## Tech Stack\n\
             Language: Zig\n\
             Build: zig build\n\
             Test: zig build test\n\
             Format: zig fmt\n\
             Idioms: prefer allocator-passing over global allocators;\n\
             comptime over macros; use zig build system."
            .into(),
        DetectedStack::Cpp { build } => match build {
            CppBuild::Cmake => "## Tech Stack\n\
                     Language: C/C++\n\
                     Build: cmake -B build && cmake --build build\n\
                     Test: ctest --test-dir build\n\
                     Idioms: prefer smart pointers over raw; use modern C++20/23;\n\
                     use fmtlib over printf."
                .into(),
            CppBuild::Meson => "## Tech Stack\n\
                     Language: C/C++\n\
                     Build: meson setup build && meson compile -C build\n\
                     Test: meson test -C build\n\
                     Idioms: prefer smart pointers over raw; use modern C++20/23."
                .into(),
            CppBuild::Make => "## Tech Stack\n\
                     Language: C/C++\n\
                     Build: make\n\
                     Test: make test\n\
                     Idioms: prefer smart pointers over raw."
                .into(),
        },
        DetectedStack::Unknown => String::new(),
    }
}

/// Read the value of a TOML key from the first matching line.
fn read_toml_field(path: std::path::PathBuf, key: &str) -> Option<String> {
    let content = std::fs::read_to_string(&path).ok()?;
    let pattern = format!("{key} = ");
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(val) = trimmed.strip_prefix(&pattern) {
            return Some(val.trim_matches('"').to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn write(root: &std::path::Path, name: &str, content: &str) {
        let mut f = fs::File::create(root.join(name)).unwrap();
        write!(f, "{}", content).unwrap();
    }

    #[test]
    fn detect_rust() {
        let d = tmpdir();
        write(d.path(), "Cargo.toml", "[package]\nedition = \"2024\"\n");
        let stack = detect(d.path());
        match stack {
            DetectedStack::Rust {
                edition,
                has_workspace,
            } => {
                assert_eq!(edition, "2024");
                assert!(!has_workspace);
            }
            _ => panic!("expected Rust"),
        }
    }

    #[test]
    fn detect_rust_workspace() {
        let d = tmpdir();
        write(
            d.path(),
            "Cargo.toml",
            "[workspace]\nmembers = [\"a\", \"b\"]\n",
        );
        let stack = detect(d.path());
        match stack {
            DetectedStack::Rust { has_workspace, .. } => assert!(has_workspace),
            _ => panic!("expected Rust workspace"),
        }
    }

    #[test]
    fn detect_python_uv() {
        let d = tmpdir();
        write(
            d.path(),
            "pyproject.toml",
            "[project]\nname = \"x\"\n[tool.ruff]\n",
        );
        let stack = detect(d.path());
        match stack {
            DetectedStack::Python { tool } => assert!(matches!(tool, PythonTool::UvPip)),
            _ => panic!("expected Python"),
        }
    }

    #[test]
    fn detect_python_poetry() {
        let d = tmpdir();
        write(d.path(), "pyproject.toml", "[tool.poetry]\nname = \"x\"\n");
        let stack = detect(d.path());
        match stack {
            DetectedStack::Python { tool } => assert!(matches!(tool, PythonTool::Poetry)),
            _ => panic!("expected Poetry"),
        }
    }

    #[test]
    fn detect_node_npm() {
        let d = tmpdir();
        write(d.path(), "package.json", "{}");
        let stack = detect(d.path());
        match stack {
            DetectedStack::Node { pkg } => assert!(matches!(pkg, PkgManager::Npm)),
            _ => panic!("expected Node"),
        }
    }

    #[test]
    fn detect_node_bun() {
        let d = tmpdir();
        write(d.path(), "package.json", "{}");
        write(d.path(), "bun.lock", "");
        let stack = detect(d.path());
        match stack {
            DetectedStack::Node { pkg } => assert!(matches!(pkg, PkgManager::Bun)),
            _ => panic!("expected Bun"),
        }
    }

    #[test]
    fn detect_unknown() {
        let d = tmpdir();
        assert!(matches!(detect(d.path()), DetectedStack::Unknown));
    }

    #[test]
    fn render_rust_contains_cargo() {
        let prompt = render_prompt(&DetectedStack::Rust {
            edition: "2021".into(),
            has_workspace: false,
        });
        assert!(prompt.contains("cargo build"));
        assert!(prompt.contains("cargo check"));
    }

    #[test]
    fn render_python_contains_pytest() {
        let prompt = render_prompt(&DetectedStack::Python {
            tool: PythonTool::UvPip,
        });
        assert!(prompt.contains("pytest"));
        assert!(prompt.contains("ruff"));
    }

    #[test]
    fn render_unknown_empty() {
        let prompt = render_prompt(&DetectedStack::Unknown);
        assert!(prompt.is_empty());
    }

    #[test]
    fn render_node_bun() {
        let prompt = render_prompt(&DetectedStack::Node {
            pkg: PkgManager::Bun,
        });
        assert!(prompt.contains("bun test"));
    }
}

//! Diagnostics feedback: after a file change, collect the compiler/lint output
//! and attach it directly to the tool result.
//!
//! Why this saves iterations: without this step the model must first make its
//! own `terminal(cargo check)` call to find out whether its
//! change compiles at all — that is a complete LLM roundtrip
//! (measured ~1.2 s median) plus tool latency, and the error only lands in the
//! context one iteration later. With feedback it is immediately in
//! the same answer.
//!
//! OFF by default, because the cost depends on the project: `cargo check` on
//! a cold `target/` takes minutes, on a warm one 1–3 s. The user
//! decides via `diagnostics.after_edit` in `~/.rijo/config.yaml`.

use crate::config::DiagnosticsConfig;
use std::path::Path;
use std::process::Command;

/// Languages/file types for which a check is worth it.
pub fn is_source_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).unwrap_or(""),
        "rs" | "ts" | "tsx" | "js" | "jsx" | "py"
    )
}

/// The check command: explicitly from the config, otherwise from the detected stack.
/// `{file}` in the config command is replaced by the changed path.
pub fn check_command(cfg: &DiagnosticsConfig, workspace: &Path, changed: &Path) -> Option<String> {
    if let Some(cmd) = cfg.command.as_ref() {
        if cmd.trim().is_empty() {
            return None;
        }
        return Some(cmd.replace("{file}", &changed.to_string_lossy()));
    }
    let file = changed.to_string_lossy().to_string();
    if workspace.join("Cargo.toml").exists() {
        return Some("cargo check --quiet --message-format short".to_string());
    }
    if workspace.join("tsconfig.json").exists() {
        return Some("npx --no-install tsc --noEmit".to_string());
    }
    if workspace.join("package.json").exists() && file.ends_with(".js") {
        return Some(format!("node --check {file}"));
    }
    if file.ends_with(".py") {
        return Some(format!("python3 -m py_compile {file}"));
    }
    None
}

/// Result of a diagnostics check, as it lands in the tool result.
pub fn run(cfg: &DiagnosticsConfig, workspace: &Path, changed: &Path) -> Option<serde_json::Value> {
    if !cfg.after_edit {
        return None;
    }
    if !is_source_file(changed) {
        return None;
    }
    let cmdline = check_command(cfg, workspace, changed)?;
    let started = std::time::Instant::now();

    // Via the shell, so that the config value may also contain pipes/arguments.
    // Plattformneutral: bash (Git Bash/MSYS) wenn verfügbar, sonst cmd.exe.
    let (shell, flag) = crate::tools::terminal_tool::shell_command();
    // `current_dir` setzt das Arbeitsverzeichnis ueber die Windows-API — die
    // Shell muss den Pfad nicht parsen, ein `\\?\...` aus `canonicalize` ist
    // damit unproblematisch.
    let wd = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let mut child = Command::new(&shell)
        .arg(flag)
        .arg(&cmdline)
        .current_dir(&wd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;

    // Timeout via its own thread: no busy wait, no process leak
    // (on overrun the child is terminated).
    let timeout = std::time::Duration::from_millis(cfg.timeout_ms.max(1_000));
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Some(serde_json::json!({
                        "command": cmdline,
                        "ok": false,
                        "timed_out": true,
                        "ms": started.elapsed().as_millis() as u64,
                        "output": format!("diagnostics aborted after {} ms (diagnostics.timeout_ms)", started.elapsed().as_millis()),
                    }));
                }
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
            Err(e) => {
                return Some(serde_json::json!({
                    "command": cmdline,
                    "ok": false,
                    "output": format!("diagnostics could not start: {e}"),
                }))
            }
        }
    }

    let out = child.wait_with_output().ok()?;
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    if !err.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&err);
    }
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let total = lines.len();
    let capped: Vec<&str> = lines.into_iter().take(cfg.max_lines.max(1)).collect();

    Some(serde_json::json!({
        "command": cmdline,
        "ok": out.status.success(),
        "ms": started.elapsed().as_millis() as u64,
        "lines": total,
        "output": capped.join("\n"),
        // So the model knows that it is not seeing the complete output.
        "truncated": total > capped.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cfg_on() -> DiagnosticsConfig {
        DiagnosticsConfig {
            after_edit: true,
            command: None,
            timeout_ms: 20_000,
            max_lines: 5,
        }
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rijo-diag-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    #[test]
    fn nur_quelldateien_werden_geprueft() {
        assert!(is_source_file(Path::new("a.rs")));
        assert!(is_source_file(Path::new("src/b.ts")));
        assert!(is_source_file(Path::new("c.py")));
        assert!(!is_source_file(Path::new("README.md")));
        assert!(!is_source_file(Path::new("config.yaml")));
        assert!(!is_source_file(Path::new("ohne_endung")));
    }

    #[test]
    fn aus_ist_aus() {
        let mut c = cfg_on();
        c.after_edit = false;
        assert!(run(&c, &tmp("off"), Path::new("x.rs")).is_none());
    }

    #[test]
    fn nicht_quellcode_wird_uebersprungen() {
        // No command, no check — and without Cargo.toml/tsconfig there is
        // nothing to detect either.
        let out = run(&cfg_on(), &tmp("nomd"), Path::new("docs/README.md"));
        assert!(out.is_none(), "{out:?}");
    }

    #[test]
    fn rust_projekt_erkennt_cargo_check() {
        let d = tmp("rust");
        std::fs::write(d.join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
        let cmd = check_command(&cfg_on(), &d, &d.join("src/lib.rs")).unwrap();
        assert!(cmd.starts_with("cargo check"), "{cmd}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn config_kommando_hat_vorrang_und_setzt_datei_ein() {
        let mut c = cfg_on();
        // `run` umhuellt `c.command` selbst mit der Shell — hier steht nur der
        // Befehlstext, mit `{file}` als Platzhalter. Der Ersatz laeuft ueber
        // bash, wenn Git Bash vorhanden ist: ein Windows-Pfad mit Backslashes
        // wuerde dort als Escape-Sequenz gelesen (`\U`, `\M`), deshalb wird der
        // Pfad mit Slashes uebergeben — beide Seiten akzeptieren das.
        let file = std::env::temp_dir()
            .join("rijo-diag-x.rs")
            .to_string_lossy()
            .replace('\\', "/");
        c.command = Some("echo {file} kaputt >&2; exit 1".to_string());
        let out = run(&c, &tmp("override"), Path::new(&file)).expect("diagnostics");
        assert_eq!(out["ok"], serde_json::json!(false));
        assert!(
            out["output"].as_str().unwrap().contains(&file),
            "the substituted path must reach the output: {out}"
        );
        assert!(
            out["command"].as_str().unwrap().contains(&file),
            "the command must carry the substituted path: {out}"
        );
    }

    #[test]
    fn ausgabe_wird_gekappt_und_als_gekappt_markiert() {
        let mut c = cfg_on();
        c.max_lines = 3;
        // `run` umhuellt selbst — nur der Befehlstext. Sequenz plattformabhaengig:
        // bash hat `seq`, cmd.exe einen For-Loop.
        let (shell, _) = crate::tools::terminal_tool::shell_command();
        let cmd = if shell == "cmd.exe" {
            "for /l %i in (1,1,20) do @echo %i"
        } else {
            "seq 1 20"
        };
        c.command = Some(cmd.to_string());
        let out = run(&c, &tmp("cap"), Path::new("x.rs")).expect("diagnostics");
        assert_eq!(out["ok"], serde_json::json!(true));
        assert_eq!(out["lines"], serde_json::json!(20));
        assert_eq!(out["truncated"], serde_json::json!(true));
        let body = out["output"].as_str().unwrap();
        assert_eq!(body.lines().count(), 3, "{body}");
    }

    #[test]
    fn timeout_bricht_ab_statt_zu_huengen() {
        let mut c = cfg_on();
        c.timeout_ms = 1_000; // minimum, so the test stays fast
                              // `run` umhuellt selbst — nur der Befehlstext. bash nutzt `sleep`,
                              // cmd.exe `ping` als Wartebefehl.
        let (shell, _) = crate::tools::terminal_tool::shell_command();
        let cmd = if shell == "cmd.exe" {
            "ping -n 30 127.0.0.1 > nul"
        } else {
            "sleep 30"
        };
        c.command = Some(cmd.to_string());
        let started = std::time::Instant::now();
        let out = run(&c, &tmp("timeout"), Path::new("x.rs")).expect("diagnostics");
        assert_eq!(out["timed_out"], serde_json::json!(true), "{out}");
        assert!(
            started.elapsed().as_secs() < 10,
            "timeout must kick in instead of waiting"
        );
    }
}

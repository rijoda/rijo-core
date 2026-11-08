//! Execution sandbox seam (ADR 0018).
//!
//! `terminal` and `execute_code` ask this module *where* their process may run.
//! The guard decides **whether to ask the user**; this decides **what an allowed
//! process may touch**. Three backends, chosen by `sandbox.backend`:
//!
//! - `off` (default) — host execution, unchanged;
//! - `auto` / `bwrap` — host execution wrapped in **bubblewrap** on Linux:
//!   read-only system, the workspace writable, a temp dir, network optional;
//! - `box` — execution inside a **Rijo Box** (`box.v1 exec.run`). The client is
//!   not wired yet, so selecting it fails **closed**: it never silently runs on
//!   the host (ADR 0018, "no silent downgrade").
//!
//! The interface is deliberately backend-agnostic so the Rijo Box backend can
//! slot in without changing `terminal`/`execute_code` (their names and schemas
//! stay — the prompt-cache invariant holds).

use crate::config::Config;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Off,
    Bwrap,
    Box,
}

/// A resolved spawn: program + args, already wrapped for the backend, plus the
/// backend label for the tool result and the audit trail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnPlan {
    pub program: String,
    pub args: Vec<String>,
    pub backend: &'static str,
}

#[derive(Debug, Clone)]
pub struct Sandbox {
    backend: Backend,
    bwrap: Option<PathBuf>,
    workspace: PathBuf,
    allow_net: bool,
}

impl Sandbox {
    /// Decide the backend from the config. `auto` uses bwrap when it is present.
    pub fn detect(config: &Config) -> Self {
        let bwrap = crate::paths::which_bin("bwrap");
        let backend = match config.sandbox.backend.trim().to_ascii_lowercase().as_str() {
            "box" => Backend::Box,
            "bwrap" => Backend::Bwrap,
            "auto" => {
                if bwrap.is_some() {
                    Backend::Bwrap
                } else {
                    Backend::Off
                }
            }
            // "off" and anything unknown: never surprise the user with a wrapper.
            _ => Backend::Off,
        };
        Self {
            backend,
            bwrap,
            workspace: config.workspace.clone(),
            allow_net: config.sandbox.network,
        }
    }

    pub fn label(&self) -> &'static str {
        match self.backend {
            Backend::Off => "off",
            Backend::Bwrap => "bwrap",
            Backend::Box => "box",
        }
    }

    /// True when no wrapping happens (the historical host path).
    pub fn is_off(&self) -> bool {
        self.backend == Backend::Off
    }

    /// Wrap one command. `Err` means: **do not execute** (fail closed).
    pub fn plan(&self, program: &str, args: &[String]) -> Result<SpawnPlan, String> {
        match self.backend {
            Backend::Off => Ok(SpawnPlan {
                program: program.to_string(),
                args: args.to_vec(),
                backend: "off",
            }),
            Backend::Bwrap => {
                let bwrap = self.bwrap.as_ref().ok_or_else(|| {
                    "sandbox.backend=bwrap, but `bwrap` (bubblewrap) is not installed".to_string()
                })?;
                let ws = self.workspace.to_string_lossy().to_string();
                let mut a: Vec<String> = vec![
                    "--die-with-parent".into(),
                    "--unshare-pid".into(),
                    // Read-only system, then the workspace writable on top, plus a
                    // private /dev, /proc and /tmp. Escape-proof enough to bound a
                    // wrong `rm -rf`; the guard still gates intent.
                    "--ro-bind".into(),
                    "/".into(),
                    "/".into(),
                    "--bind".into(),
                    ws.clone(),
                    ws,
                    "--tmpfs".into(),
                    "/tmp".into(),
                    "--dev".into(),
                    "/dev".into(),
                    "--proc".into(),
                    "/proc".into(),
                ];
                if !self.allow_net {
                    a.push("--unshare-net".into());
                }
                a.push("--".into());
                a.push(program.to_string());
                a.extend(args.iter().cloned());
                Ok(SpawnPlan {
                    program: bwrap.to_string_lossy().to_string(),
                    args: a,
                    backend: "bwrap",
                })
            }
            Backend::Box => Err(
                "sandbox.backend=box: execution inside a Rijo Box is not wired yet \
                 (`box.v1 exec.run` client pending). Failing closed instead of running \
                 on the host."
                    .to_string(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(backend: Backend, bwrap: Option<&str>, allow_net: bool) -> Sandbox {
        Sandbox {
            backend,
            bwrap: bwrap.map(PathBuf::from),
            workspace: PathBuf::from("/work/ws"),
            allow_net,
        }
    }

    #[test]
    fn off_passes_the_command_through_unchanged() {
        let s = sandbox(Backend::Off, None, true);
        let p = s
            .plan("bash", &["-c".to_string(), "echo hi".to_string()])
            .unwrap();
        assert_eq!(p.program, "bash");
        assert_eq!(p.args, vec!["-c", "echo hi"]);
        assert_eq!(p.backend, "off");
    }

    #[test]
    fn box_fails_closed() {
        let s = sandbox(Backend::Box, None, true);
        assert!(
            s.plan("bash", &["-c".to_string(), "echo hi".to_string()])
                .is_err(),
            "the box backend must not silently run on the host"
        );
    }

    #[test]
    fn bwrap_wraps_with_readonly_root_and_writable_workspace() {
        let s = sandbox(Backend::Bwrap, Some("/usr/bin/bwrap"), true);
        let p = s
            .plan("bash", &["-c".to_string(), "echo hi".to_string()])
            .unwrap();
        assert_eq!(p.program, "/usr/bin/bwrap");
        assert_eq!(p.backend, "bwrap");
        let joined = p.args.join(" ");
        assert!(joined.contains("--ro-bind / /"), "{joined}");
        assert!(joined.contains("--bind /work/ws /work/ws"), "{joined}");
        // Network is allowed here, so it must NOT be unshared.
        assert!(!joined.contains("--unshare-net"), "{joined}");
        // The original program + args still come last, after `--`.
        assert_eq!(
            &p.args[p.args.len() - 3..],
            &["bash".to_string(), "-c".to_string(), "echo hi".to_string()]
        );
    }

    #[test]
    fn bwrap_can_deny_the_network() {
        let s = sandbox(Backend::Bwrap, Some("bwrap"), false);
        let p = s
            .plan("bash", &["-c".to_string(), "curl https://x".to_string()])
            .unwrap();
        assert!(p.args.join(" ").contains("--unshare-net"));
    }

    #[test]
    fn bwrap_without_the_binary_is_an_error_not_a_silent_host_run() {
        let s = sandbox(Backend::Bwrap, None, true);
        assert!(s
            .plan("bash", &["-c".to_string(), "echo hi".to_string()])
            .is_err());
    }
}

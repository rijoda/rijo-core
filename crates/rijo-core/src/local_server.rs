//! Local model server lifecycle for llama.cpp.
//!
//! Rijo Code talks to any OpenAI-compatible endpoint via `base_url` + `api_key`.
//! A local model is no different — except that the *client* should own the
//! server process, because two measured effects only exist if the server is
//! long-lived:
//!
//! 1. **Prefix KV reuse.** llama-server keeps the KV cache of the shared prefix
//!    (system prompt + history). Measured on this machine (Qwen3-30B-A3B, CPU):
//!    a 670-token prefix cost 28.9 s cold and **0.88 s warm** — 33x less prefill.
//!    Starting a process per request throws that away.
//! 2. **Prompt compression.** A terse system prompt (39 tokens) prefilled in
//!    1.8 s versus 8.0 s for the verbose one (117 tokens).
//!
//! This module provides the profile policy (which flags for which model, taken
//! from `projects/models/quantisation/hebel-analyse.md`) and a small owning
//! handle around the `llama-server` child process. It deliberately uses only
//! `std` so it compiles everywhere and is trivially unit-testable.

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Default port when the caller does not pick one.
pub const DEFAULT_PORT: u16 = 8080;

/// Flags for one `llama-server` invocation, derived from measured profiles.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalProfile {
    pub threads: u32,
    /// Layers on the GPU backend (`-ngl`). 0 = CPU only; a small positive value
    /// is the measured sweet spot for models that fit the iGPU.
    pub n_gpu_layers: i32,
    pub ubatch: u32,
    pub flash_attn: bool,
    pub ctx: u32,
    /// Quantize the KV cache (`-ctk/-ctv q8_0`). Saves RAM, costs speed.
    pub cache_q8: bool,
    /// `--cache-reuse`: minimum chunk size reused from the prefix KV cache.
    pub cache_reuse: u32,
    /// `-np 1`: one slot. Agent runs are sequential, and one slot keeps the
    /// prefix cache from being rotated out by parallel requests.
    pub parallel: u32,
}

impl Default for LocalProfile {
    fn default() -> Self {
        Self {
            threads: 8,
            n_gpu_layers: 0,
            ubatch: 512,
            flash_attn: true,
            ctx: 32_768,
            cache_q8: false,
            cache_reuse: 256,
            parallel: 1,
        }
    }
}

/// The measured policy: which server flags for which local model.
///
/// `size_gib` is the weight file size. The rules come from the lever analysis on
/// this machine (Intel Core Ultra 7 255U, 32 GB, iGPU + CPU):
/// * large MoE (>= ~14 GiB, e.g. Qwen3-30B-A3B 17.3 GiB): the iGPU cannot
///   allocate the prefill buffers -> CPU only.
/// * a MoE that fits the iGPU (< ~13 GiB, e.g. gpt-oss-20b 10.8 GiB): `-ngl 8`
///   jumps prefill 4.3x for only ~9% decode loss; ubatch 1024 is best.
/// * everything else small/dense: CPU only.
pub fn profile_for(model_name: &str, size_gib: f64) -> LocalProfile {
    let name = model_name.to_ascii_lowercase();
    let mut p = LocalProfile::default();
    if size_gib >= 14.0 {
        // Large MoE: iGPU offload fails; keep the measured CPU decode shape.
        p.n_gpu_layers = 0;
    } else if name.contains("gpt-oss") {
        p.n_gpu_layers = 8;
        p.ubatch = 1024;
    } else {
        p.n_gpu_layers = 0;
    }
    p
}

/// Builds the `llama-server` argument vector (without the exe itself).
pub fn server_args(model: &Path, port: u16, p: &LocalProfile) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-m".into(),
        model.to_string_lossy().into_owned(),
        "-t".into(),
        p.threads.to_string(),
        "-ngl".into(),
        p.n_gpu_layers.to_string(),
        "-fa".into(),
        if p.flash_attn { "on" } else { "off" }.into(),
        "-ub".into(),
        p.ubatch.to_string(),
        "-c".into(),
        p.ctx.to_string(),
        "--port".into(),
        port.to_string(),
        "--host".into(),
        "127.0.0.1".into(),
        "--cache-reuse".into(),
        p.cache_reuse.to_string(),
        "-np".into(),
        p.parallel.to_string(),
    ];
    if p.cache_q8 {
        a.extend(["-ctk".into(), "q8_0".into(), "-ctv".into(), "q8_0".into()]);
    }
    a
}

/// The OpenAI-compatible base URL of a local server on `port`.
pub fn base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/v1")
}

/// Whether a base URL points at a local server (localhost / 127.0.0.1 / ::1).
pub fn is_local_base(base_url: &str) -> bool {
    let b = base_url.trim();
    b.starts_with("http://127.0.0.1")
        || b.starts_with("http://localhost")
        || b.starts_with("http://[::1]")
}

/// Convenience: derive the profile from the model file (name + size) and start.
///
/// This is what a CLI `--local <model.gguf>` uses: the caller names one file and
/// gets the measured flags for it.
pub fn start_for_model(server_exe: &Path, model: &Path, port: u16) -> std::io::Result<LocalServer> {
    let size_gib = std::fs::metadata(model)
        .map(|m| m.len() as f64 / (1024.0 * 1024.0 * 1024.0))
        .unwrap_or(0.0);
    let name = model.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let profile = profile_for(name, size_gib);
    LocalServer::start(server_exe, model, &profile, port)
}

/// An owned `llama-server` child process.
///
/// Dropping the value kills the process — the owner decides the lifetime, not
/// the OS. `wait_ready` polls the TCP port until the server accepts connections.
pub struct LocalServer {
    child: Child,
    port: u16,
    exe: PathBuf,
}

impl LocalServer {
    /// Spawns `llama-server` with the given profile. Returns once the process is
    /// created; call [`wait_ready`](Self::wait_ready) before sending requests.
    pub fn start(
        server_exe: &Path,
        model: &Path,
        profile: &LocalProfile,
        port: u16,
    ) -> std::io::Result<Self> {
        let child = Command::new(server_exe)
            .args(server_args(model, port, profile))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Self {
            child,
            port,
            exe: server_exe.to_path_buf(),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn exe(&self) -> &Path {
        &self.exe
    }

    pub fn base_url(&self) -> String {
        base_url(self.port)
    }

    /// Polls the port until the server is up, the child exits, or `timeout`
    /// elapses. Returns true only when the server accepted a connection.
    pub fn wait_ready(&mut self, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if self.child.try_wait().ok().flatten().is_some() {
                return false; // the process died (bad model path, bad flags)
            }
            if TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        false
    }
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_large_moe_is_cpu_only() {
        // Qwen3-30B-A3B Q4 is 17.3 GiB: the iGPU cannot take it.
        let p = profile_for("Qwen3-30B-A3B-Q4_K_M.gguf", 17.3);
        assert_eq!(p.n_gpu_layers, 0);
    }

    #[test]
    fn a_moe_that_fits_gets_the_measured_sweet_spot() {
        // gpt-oss-20b Q4 is 10.8 GiB: -ngl 8, ubatch 1024 (measured).
        let p = profile_for("gpt-oss-20b-Q4_K_M.gguf", 10.8);
        assert_eq!(p.n_gpu_layers, 8);
        assert_eq!(p.ubatch, 1024);
    }

    #[test]
    fn a_small_dense_model_stays_on_the_cpu() {
        let p = profile_for("qwen2.5-1.5b-instruct-q4_k_m.gguf", 1.0);
        assert_eq!(p.n_gpu_layers, 0);
        assert_eq!(p.ubatch, 512);
    }

    #[test]
    fn server_args_always_carry_prefix_reuse_and_one_slot() {
        let p = profile_for("gpt-oss-20b", 10.8);
        let a = server_args(Path::new("/models/x.gguf"), 8080, &p);
        let joined = a.join(" ");
        assert!(joined.contains("--cache-reuse 256"), "args: {joined}");
        assert!(joined.contains("-np 1"), "args: {joined}");
        assert!(joined.contains("-fa on"), "args: {joined}");
        assert!(joined.contains("-ngl 8"), "args: {joined}");
        // The api key is never part of the server arguments.
        assert!(!joined.to_ascii_lowercase().contains("key"));
    }

    #[test]
    fn cache_quantization_is_only_added_on_request() {
        let mut p = LocalProfile::default();
        let plain = server_args(Path::new("m.gguf"), 8080, &p).join(" ");
        assert!(!plain.contains("-ctk"));
        p.cache_q8 = true;
        let quantized = server_args(Path::new("m.gguf"), 8080, &p).join(" ");
        assert!(quantized.contains("-ctk q8_0"));
        assert!(quantized.contains("-ctv q8_0"));
    }

    #[test]
    fn base_url_and_local_detection() {
        assert_eq!(base_url(8080), "http://127.0.0.1:8080/v1");
        assert!(is_local_base("http://127.0.0.1:8080/v1"));
        assert!(is_local_base("http://localhost:11434/v1"));
        assert!(!is_local_base("https://api.deepseek.com/v1"));
    }

    #[test]
    fn a_missing_server_binary_is_an_error_not_a_panic() {
        let missing = Path::new("/definitely/not/llama-server");
        let r = LocalServer::start(missing, Path::new("m.gguf"), &LocalProfile::default(), 0);
        assert!(r.is_err());
    }

    #[test]
    fn start_for_model_reports_a_missing_server() {
        let r = start_for_model(
            Path::new("/definitely/not/llama-server"),
            Path::new("m.gguf"),
            0,
        );
        assert!(r.is_err());
    }
}

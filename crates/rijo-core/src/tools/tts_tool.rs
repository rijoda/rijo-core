//! TTS tool — LOCAL ONLY. No cloud, no network path.
//!
//! Chain (first voice found wins):
//! 1. `piper` — neural local synthesis (best result); the voice model is a
//!    `.onnx` under `~/.rijo/piper-voices/`
//! 2. `espeak-ng` — repo package, built-in voices, usable offline right away
//! 3. `espeak` — legacy fallback
//!
//! Deliberately NOT supported: edge-tts and other cloud services. The text
//! does not leave the machine; no HTTP client is used.

use crate::tools::{tool_error, AsyncTool, JsonSchema, ToolMeta, ToolResult};
use crate::AppState;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

pub struct TextToSpeech;

const MAX_TEXT: usize = 4000;

#[derive(Debug, Clone, Copy, PartialEq)]
enum LocalBackend {
    Piper,
    EspeakNg,
    Espeak,
}

impl LocalBackend {
    fn name(&self) -> &'static str {
        match self {
            LocalBackend::Piper => "piper",
            LocalBackend::EspeakNg => "espeak-ng",
            LocalBackend::Espeak => "espeak",
        }
    }
}

/// Is a binary in the PATH? No subprocess, no `which` (which Windows lacks).
fn have(bin: &str) -> bool {
    crate::paths::which_bin(bin).is_some()
}

/// Looks for a local piper voice model (.onnx).
/// Order: RIJO_PIPER_MODEL → ~/.rijo/piper-voices/ → ~/.local/share/piper-voices/
fn find_piper_model() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("RIJO_PIPER_MODEL") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let dirs = [
        crate::paths::rijo_home().join("piper-voices"),
        crate::paths::home_dir()
            .join(".local")
            .join("share")
            .join("piper-voices"),
    ];
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut models: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "onnx").unwrap_or(false))
            .collect();
        models.sort();
        if let Some(m) = models.into_iter().next() {
            return Some(m);
        }
    }
    None
}

/// First available local backend (piper only WITH a voice model).
fn pick_backend() -> Option<LocalBackend> {
    if have("piper") && find_piper_model().is_some() {
        return Some(LocalBackend::Piper);
    }
    if have("espeak-ng") {
        return Some(LocalBackend::EspeakNg);
    }
    if have("espeak") {
        return Some(LocalBackend::Espeak);
    }
    None
}

fn install_hint() -> String {
    "No local TTS backend found. Local-only options:\n\
     · sudo pacman -S espeak-ng            (repo, built-in voices, works offline immediately)\n\
     · yay -S piper-tts-bin                (better quality) + a voice model as .onnx in ~/.rijo/piper-voices/\n\
     Cloud backends (edge-tts and friends) are deliberately not used."
        .to_string()
}

#[async_trait]
impl AsyncTool for TextToSpeech {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "text_to_speech",
            description:
                "Local speech synthesis, offline (piper -> espeak-ng); returns the WAV path.",
            emoji: "🔊",
            toolset: "core",
            is_async: true,
        }
    }

    fn schema(&self) -> &JsonSchema {
        &TTS_SCHEMA
    }

    async fn call_async(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        let text = match args.get("text").and_then(|v| v.as_str()) {
            Some(t) if !t.trim().is_empty() => t.to_string(),
            _ => return tool_error("text is required"),
        };
        let text: String = text.chars().take(MAX_TEXT).collect();

        let backend = match pick_backend() {
            Some(b) => b,
            None => return tool_error(install_hint()),
        };

        let out_dir = state.config.data_dir.join("audio");
        let _ = std::fs::create_dir_all(&out_dir);
        let output: PathBuf = out_dir.join(format!("tts_{}.wav", uuid::Uuid::new_v4().simple()));
        let out_str = output.to_string_lossy().to_string();

        // Voice: config.tts.voice; empty = a sensible default per backend.
        let voice = state.config.tts.voice.trim().to_string();
        let text_len = text.chars().count();

        let result = match backend {
            LocalBackend::Piper => {
                let model = match find_piper_model() {
                    Some(m) => m,
                    None => return tool_error("piper installed, but no voice model found (RIJO_PIPER_MODEL or ~/.rijo/piper-voices/*.onnx)"),
                };
                let mut cmd = tokio::process::Command::new("piper");
                cmd.arg("--model")
                    .arg(&model)
                    .arg("--output_file")
                    .arg(&out_str);
                if !voice.is_empty() {
                    cmd.arg("--speaker").arg(&voice);
                }
                cmd.stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());
                let mut child = match cmd.spawn() {
                    Ok(c) => c,
                    Err(e) => return tool_error(format!("piper start failed: {e}")),
                };
                if let Some(mut stdin) = child.stdin.take() {
                    use tokio::io::AsyncWriteExt;
                    let _ = stdin.write_all(text.as_bytes()).await;
                    let _ = stdin.shutdown().await;
                }
                match child.wait_with_output().await {
                    Ok(o) if o.status.success() && output.exists() => Ok(()),
                    Ok(o) => Err(format!(
                        "piper failed: {}",
                        String::from_utf8_lossy(&o.stderr)
                            .chars()
                            .take(200)
                            .collect::<String>()
                    )),
                    Err(e) => Err(format!("piper: {e}")),
                }
            }
            LocalBackend::EspeakNg | LocalBackend::Espeak => {
                let bin = backend.name();
                let lang = if voice.is_empty() {
                    "de".to_string()
                } else {
                    voice.clone()
                };
                let out = tokio::process::Command::new(bin)
                    .arg("-v")
                    .arg(&lang)
                    .arg("-w")
                    .arg(&out_str)
                    .arg(&text)
                    .output()
                    .await;
                match out {
                    Ok(o) if o.status.success() && output.exists() => Ok(()),
                    Ok(o) => Err(format!(
                        "{bin} failed: {}",
                        String::from_utf8_lossy(&o.stderr)
                            .chars()
                            .take(200)
                            .collect::<String>()
                    )),
                    Err(e) => Err(format!("{bin}: {e}")),
                }
            }
        };

        match result {
            Ok(()) => json!({
                "audio_path": out_str,
                "backend": backend.name(),
                "chars": text_len,
                "local_only": true,
                "message": "speech generated locally (no network)"
            })
            .to_string(),
            Err(e) => {
                let _ = std::fs::remove_file(&output);
                tool_error(e)
            }
        }
    }
}

/// For tests/docs only: does a local backend exist?
pub fn local_backend_available() -> bool {
    pick_backend().is_some()
}

lazy_static::lazy_static! {
    static ref TTS_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "text": { "type": "string", "description": "Text (max 4000 chars)" },
            "instructions": { "type": "string", "description": "Ignored (local only)" }
        },
        "required": ["text"]
    }));
}

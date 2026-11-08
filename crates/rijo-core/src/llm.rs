//! LLM adapter — OpenAI-compatible chat completions (streaming + retry + guards)

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: Option<String>,

    /// DeepSeek thinking mode: chain-of-thought. MUST be passed back on
    /// subsequent tool-bearing requests (else the API returns 400).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,

    /// Multimodal content parts (text + `image_url`), in the OpenAI shape. When
    /// set, `content` is serialized as THIS array instead of a plain string —
    /// that is what a vision model expects. Never comes back from a provider and
    /// is not persisted; a resumed session keeps the text, not the pixels.
    #[serde(skip)]
    pub parts: Option<Vec<Value>>,
}

/// Hand-written so ONE struct can put out `content` either as a string (ordinary
/// turn) or as the parts array (turn with attached images) — a `#[derive]` cannot
/// switch a field's type at runtime. Field order is kept explicit (not a map), so
/// the request body stays byte-stable, which the prompt cache depends on.
impl Serialize for ChatMessage {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        #[serde(untagged)]
        enum Content<'a> {
            Text(Option<&'a String>),
            Parts(&'a Vec<Value>),
        }

        #[derive(Serialize)]
        struct Wire<'a> {
            role: &'a str,
            content: Content<'a>,
            #[serde(skip_serializing_if = "Option::is_none")]
            reasoning_content: Option<&'a String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            tool_calls: Option<&'a Vec<ToolCall>>,
            #[serde(skip_serializing_if = "Option::is_none")]
            tool_call_id: Option<&'a String>,
        }

        Wire {
            role: &self.role,
            content: match &self.parts {
                Some(parts) => Content::Parts(parts),
                None => Content::Text(self.content.as_ref()),
            },
            reasoning_content: self.reasoning_content.as_ref(),
            tool_calls: self.tool_calls.as_ref(),
            tool_call_id: self.tool_call_id.as_ref(),
        }
        .serialize(s)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub stream: Option<bool>,
    pub tools: Option<Vec<Value>>,
    /// DeepSeek V4 thinking mode — required for reasoning_content output.
    /// Skipped when None so other providers ignore it entirely.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Value>,
    /// Reasoning effort (DeepSeek thinking models): low/high/max.
    /// REQUIRED in practice — without it DeepInfra delivers no reasoning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Stream options: `{"include_usage": true}` delivers the token usage
    /// in the last SSE chunk. Without it the usage in the stream is unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<Value>,
}

fn request_reasoning_effort(
    model: &str,
    configured_effort: &str,
    per_turn_effort: Option<&str>,
) -> Option<String> {
    if !model.to_lowercase().contains("deepseek") {
        return None;
    }
    Some(match per_turn_effort.unwrap_or(configured_effort) {
        "min" => "low".to_string(),
        "max" => "max".to_string(),
        _ => "medium".to_string(),
    })
}

fn request_reasoning_fields(
    model: &str,
    configured_effort: &str,
    per_turn_effort: Option<&str>,
) -> (Option<Value>, Option<String>) {
    let lower_model = model.to_lowercase();
    let thinking = (lower_model.contains("deepseek") || lower_model.contains("glm"))
        .then(|| serde_json::json!({"type": "enabled"}));
    let effort = request_reasoning_effort(model, configured_effort, per_turn_effort);
    (thinking, effort)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub choices: Vec<Choice>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    pub message: Option<ResponseMessage>,
    pub delta: Option<Delta>,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseMessage {
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    pub tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Delta {
    pub content: Option<String>,
    pub tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    /// DeepSeek context caching: how many prompt tokens came from the cache
    /// (much faster/cheaper than newly processed ones). Missing with other
    /// providers → 0.
    #[serde(default)]
    pub prompt_cache_hit_tokens: u32,
    #[serde(default)]
    pub prompt_cache_miss_tokens: u32,
}

/// Token usage of an LLM call (prompt + completion).
/// Measured, not estimated — the stream delivers usage in the last chunk
/// when `stream_options.include_usage` is set.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TokenUsage {
    pub prompt: u32,
    pub completion: u32,
    /// Prompt tokens from the provider cache (prefix caching). 0 = the provider
    /// reports nothing (then the hit rate is unknown, not zero).
    pub cache_hit: u32,
    pub cache_miss: u32,
}

impl TokenUsage {
    pub fn total(&self) -> u32 {
        self.prompt + self.completion
    }

    /// Share of prompt tokens that came from the cache (0.0 … 1.0).
    /// None when the provider delivers no cache information.
    pub fn cache_hit_ratio(&self) -> Option<f64> {
        let seen = self.cache_hit + self.cache_miss;
        if seen == 0 {
            None
        } else {
            Some(self.cache_hit as f64 / seen as f64)
        }
    }
}

/// Streaming delta kind: content (the answer) or reasoning (thinking text).
#[derive(Debug, Clone)]
pub enum StreamDelta {
    /// Visible answer text.
    Content { text: String },
    /// Reasoning/thinking text (reasoning_content / reasoning field).
    Reasoning { text: String },
    /// Tool-call argument generation progress: grows as the model streams args.
    ToolArgs {
        /// Tool name so far (may still be empty at first arg chunks)
        tool: String,
        /// Accumulated argument chars for this tool call so far
        total_chars: usize,
        /// Best-effort human target (path/command/query…) from the partial args,
        /// so the UI can show *what* is being written instead of a char count.
        preview: String,
    },
}

/// Best-effort human target of a tool call from its (possibly still partial)
/// argument JSON: the value of the first `path`/`command`/`query`/… key found.
///
/// A partial JSON cannot be parsed, so this scans for the key and takes the
/// following string literal. Purely cosmetic — an empty result means "no target
/// visible yet", never an error.
pub fn args_target(args: &str) -> Option<String> {
    const KEYS: [&str; 8] = [
        "path",
        "file_path",
        "command",
        "query",
        "url",
        "repo",
        "pattern",
        "name",
    ];
    for key in KEYS {
        let needle = format!("\"{key}\"");
        let Some(i) = args.find(&needle) else {
            continue;
        };
        let rest = &args[i + needle.len()..];
        let Some(colon) = rest.find(':') else {
            continue;
        };
        let after = rest[colon + 1..].trim_start();
        let Some(stripped) = after.strip_prefix('"') else {
            continue;
        };
        let val: String = stripped.chars().take_while(|c| *c != '"').collect();
        if val.is_empty() {
            continue;
        }
        let flat = val.replace('\\', "/");
        let n = flat.chars().count();
        // Keep the tail of a long path: "…/tools/web_tools.rs" is the useful part.
        return Some(if n > 40 {
            format!("…{}", flat.chars().skip(n - 38).collect::<String>())
        } else {
            flat
        });
    }
    None
}

/// Streaming callback: receives incremental deltas (content + reasoning).
pub type StreamCallback<'a> = &'a (dyn Fn(&StreamDelta) + Send + Sync);

// ─── Client ──────────────────────────────────────────────────────────────

use crate::config::LlmConfig;
use crate::AppState;

pub struct LlmClient {
    pub(crate) http: reqwest::Client,
    pub(crate) base_url: String,
    pub(crate) api_key: String,
    pub(crate) model: String,
    pub(crate) max_tokens: u32,
    pub(crate) temperature: f32,
    pub(crate) thinking_effort: String,
    /// Per-process session id, sent when the provider needs one (OpenCode:
    /// a missing `x-opencode-session` is answered with HTTP 400
    /// "MissingSessionID").
    pub(crate) session_header: String,
    pub(crate) session_id: String,
    pub(crate) extra_headers: Vec<(String, String)>,
}

/// Parses `Name: value` lines. A malformed line is dropped, never panic —
/// headers come from a config file the user types.
pub fn parse_header_lines(lines: &[String]) -> Vec<(String, String)> {
    lines
        .iter()
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            let name = name.trim();
            let value = value.trim();
            (!name.is_empty() && !value.is_empty()).then(|| (name.to_string(), value.to_string()))
        })
        .collect()
}

/// Turn a local image path into a data URL; pass an http(s) URL through.
/// Shared by `vision_analyze` and the TUI's attachment path — one place that
/// knows how an image reaches the provider.
pub fn image_ref(path_or_url: &str) -> Result<String, String> {
    if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
        return Ok(path_or_url.to_string());
    }
    let path = shellexpand::tilde(path_or_url).to_string();
    let bytes = std::fs::read(&path).map_err(|e| format!("cannot read image {path}: {e}"))?;
    let mime = match std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("bmp") => "image/bmp",
        _ => "image/jpeg",
    };
    Ok(format!(
        "data:{mime};base64,{}",
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes)
    ))
}

/// Whether a path looks like an image a vision model can take.
pub fn is_image_path(p: &str) -> bool {
    matches!(
        std::path::Path::new(p)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp")
    )
}

/// Cap for an inlined text attachment. Beyond this the text part would crowd out
/// the actual task; the cut is marked in the prompt so the model knows.
pub const MAX_ATTACH_CHARS: usize = 20_000;

/// A file the user attached to a message. Built once, at send time.
#[derive(Debug, Clone)]
pub struct Attachment {
    /// The path as the user typed it — goes into the note in the prompt.
    pub path: String,
    /// Image → data URL, sent as an `image_url` part.
    pub data_url: Option<String>,
    /// Text file → the (capped) content, inlined into the text part.
    pub text: Option<String>,
    /// Set when `text` was cut at [`MAX_ATTACH_CHARS`].
    pub truncated: bool,
}

impl Attachment {
    /// Is this an image (as opposed to an inlined text file)?
    pub fn is_image(&self) -> bool {
        self.data_url.is_some()
    }
}

/// Read one attachment from disk. Never panics: a failure comes back as `Err`
/// text so the caller can report it and still run the turn without that file.
pub fn load_attachment(path: &str) -> Result<Attachment, String> {
    if is_image_path(path) {
        Ok(Attachment {
            path: path.to_string(),
            data_url: Some(image_ref(path)?),
            text: None,
            truncated: false,
        })
    } else {
        let content = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        let truncated = content.chars().count() > MAX_ATTACH_CHARS;
        Ok(Attachment {
            path: path.to_string(),
            data_url: None,
            text: Some(content.chars().take(MAX_ATTACH_CHARS).collect()),
            truncated,
        })
    }
}

/// Whether the configured model can take an image as an `image_url` part.
///
/// A heuristic, not a contract — providers expose no capability endpoint. It errs
/// to "no" for text-only families (DeepSeek chat/reasoner) so an attached image
/// becomes a clear note instead of a provider-side 400. `RIJO_VISION=on|off`
/// overrides the guess for a model the list does not know.
pub fn model_supports_vision(model: &str) -> bool {
    match std::env::var("RIJO_VISION")
        .ok()
        .map(|v| v.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("on" | "yes" | "1" | "true") => return true,
        Some("off" | "no" | "0" | "false") => return false,
        _ => {}
    }
    let m = model.to_ascii_lowercase();
    const VISION: &[&str] = &[
        "vl",
        "vision",
        "llava",
        "pixtral",
        "internvl",
        "gpt-4o",
        "gpt-4.1",
        "gpt-4-turbo",
        "gpt-5",
        "o1",
        "o3",
        "o4",
        "claude",
        "gemini",
        "grok-2",
        "grok-3",
        "grok-4",
        "llama-3.2",
        "llama-4",
        "gemma-3",
        "phi-3-vision",
        "phi-4",
    ];
    VISION.iter().any(|k| m.contains(k))
}

/// Retryable LLM call wrapper: exponential backoff on 429/5xx/network.
const RETRY_DELAYS: [u64; 3] = [1, 3, 8];

fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504 | 529)
}

/// How long a single chat request may take.
///
/// A local `llama-server` prefills the whole prompt on the CPU before the first
/// token. With the full tool schema set that is ~8.4k tokens, measured at
/// 50–100 t/s on this laptop, and a thinking model then generates slowly. The
/// cloud value (180 s) cancelled exactly that case, retried with a fresh prefill,
/// and cancelled again — the turn could never finish. 15 min is a ceiling for a
/// hang, not an expectation.
fn request_timeout(base_url: &str) -> std::time::Duration {
    let secs = if crate::local_server::is_local_base(base_url) {
        900
    } else {
        180
    };
    std::time::Duration::from_secs(secs)
}

impl LlmClient {
    pub fn new(cfg: &LlmConfig) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(request_timeout(&cfg.base_url))
                .build()
                .unwrap_or_default(),
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            api_key: cfg.api_key.clone(),
            model: cfg.model.clone(),
            max_tokens: cfg.max_tokens,
            temperature: cfg.temperature,
            thinking_effort: cfg.thinking_effort.clone(),
            session_header: cfg.session_header.trim().to_string(),
            // Per process, not per request: OpenCode uses it for routing, and a
            // stable id within a run keeps one conversation on one backend.
            session_id: format!("rijo-{}", std::process::id()),
            extra_headers: parse_header_lines(&cfg.extra_headers),
        }
    }

    /// Applies the provider's extra headers to a request (session header first,
    /// so an explicit extra header can override it).
    fn apply_headers(&self, mut req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if !self.session_header.is_empty() {
            req = req.header(&self.session_header, &self.session_id);
        }
        for (name, value) in &self.extra_headers {
            req = req.header(name, value);
        }
        req
    }

    /// One-shot multimodal call: ask the model about an image (data URL or http
    /// URL). Shares this client's headers, so a provider that needs a session
    /// header (OpenCode) works here too — the old vision path built its own
    /// request and would have failed there.
    pub async fn describe_image(&self, image_ref: &str, question: &str) -> Result<String, String> {
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": 1024,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": question},
                    {"type": "image_url", "image_url": {"url": image_ref}}
                ]
            }]
        });
        let req = self
            .apply_headers(
                self.http
                    .post(format!("{}/chat/completions", self.base_url))
                    .header("Authorization", format!("Bearer {}", self.api_key)),
            )
            .json(&body);
        let resp = req.send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!(
                "vision error {status}: {}",
                text.chars().take(300).collect::<String>()
            ));
        }
        let v: Value =
            serde_json::from_str(&text).map_err(|e| format!("bad vision response: {e}"))?;
        Ok(v["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string())
    }

    /// Set the temperature at runtime. The schema evaluation needs 0 so that
    /// the variants are comparable at all.
    pub fn set_temperature(&mut self, t: f32) {
        self.temperature = t;
    }

    /// One completion — non-streaming. Returns (text, reasoning, tool_calls, tokens).
    pub async fn chat(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
    ) -> Result<(String, String, Vec<ToolCall>, Option<TokenUsage>), String> {
        self.chat_streaming(messages, tools, None).await
    }

    /// Completion with optional live streaming callback (text deltas).
    /// Tool-calls still arrive only at the end (deltas merged via index).
    /// Returns (text, reasoning, tool_calls, usage) — reasoning is the
    /// full chain-of-thought, REQUIRED for pass-back in tool-bearing turns.
    pub async fn chat_streaming(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
        on_delta: Option<StreamCallback<'_>>,
    ) -> Result<(String, String, Vec<ToolCall>, Option<TokenUsage>), String> {
        self.chat_streaming_with_effort(messages, tools, on_delta, None)
            .await
    }

    /// Like [`Self::chat_streaming`], with an ephemeral per-turn effort override.
    /// This avoids mutating the shared client/config while a turn is in flight.
    pub async fn chat_streaming_with_effort(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
        on_delta: Option<StreamCallback<'_>>,
        per_turn_effort: Option<&str>,
    ) -> Result<(String, String, Vec<ToolCall>, Option<TokenUsage>), String> {
        let want_stream = on_delta.is_some();

        // ── Offline mode: deterministic scripted turns ────────────────────
        // RIJO_LLM_MOCK=<path to json> replays a fixed turn sequence so the
        // whole tool loop (dispatch → tool result → next turn) can be
        // verified without network access or API credit. Nothing about the
        // LLM itself is exercised — only the harness around it.
        if let Some(script) = mock_script() {
            return mock_turn(script, on_delta);
        }

        // Every return site assigns before it reads, so the initial value is
        // never read — the lint is about the "dead" init only.
        #[allow(unused_assignments)]
        let mut last_err = String::new();
        'retry: for attempt in 0..=RETRY_DELAYS.len() {
            let (thinking, reasoning_effort) =
                request_reasoning_fields(&self.model, &self.thinking_effort, per_turn_effort);
            let req = ChatRequest {
                model: self.model.clone(),
                messages: messages.to_vec(),
                temperature: Some(self.temperature),
                max_tokens: Some(self.max_tokens),
                stream: Some(want_stream),
                // Token usage in the stream too: otherwise it is unknown
                stream_options: if want_stream {
                    Some(serde_json::json!({"include_usage": true}))
                } else {
                    None
                },
                tools: if tools.is_empty() {
                    None
                } else {
                    Some(tools.to_vec())
                },
                // DeepSeek V4 (DeepInfra): thinking-only models need this
                // explicit flag AND reasoning_effort — without effort the
                // model silently produces NO reasoning_content (verified via
                // A/B test 2026-09-06). Other providers see None → omitted.
                thinking,
                // TIERED effort (min|med|max). Research-backed default med hits
                // the accuracy/latency knee; a TUI turn may override it without
                // mutating the shared LLM client.
                reasoning_effort,
            };

            let resp = self
                .http
                .post(format!("{}/chat/completions", self.base_url))
                .header("Authorization", format!("Bearer {}", self.api_key))
                .header("Content-Type", "application/json")
                .json(&req);
            let resp = self.apply_headers(resp).send().await;

            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    last_err = format!("LLM request failed: {e}");
                    if attempt < RETRY_DELAYS.len() {
                        tokio::time::sleep(std::time::Duration::from_secs(
                            RETRY_DELAYS.get(attempt).copied().unwrap_or(0),
                        ))
                        .await;
                        continue;
                    }
                    return Err(last_err);
                }
            };

            if !resp.status().is_success() {
                let status = resp.status().as_u16();
                let body = resp.text().await.unwrap_or_default();
                last_err = format!(
                    "LLM API error {status}: {}",
                    body.chars().take(300).collect::<String>()
                );
                if is_retryable_status(status) && attempt < RETRY_DELAYS.len() {
                    tokio::time::sleep(std::time::Duration::from_secs(
                        RETRY_DELAYS.get(attempt).copied().unwrap_or(0),
                    ))
                    .await;
                    continue;
                }
                return Err(last_err);
            }

            if !want_stream {
                // Body decode can fail in the middle of the body (proxy reset etc.).
                // The request is idempotent — so retry as in the stream path.
                let parsed: ChatResponse = match resp.json().await {
                    Ok(p) => p,
                    Err(e) => {
                        let err = format!("Bad LLM response: {e}");
                        if attempt < RETRY_DELAYS.len() {
                            tokio::time::sleep(std::time::Duration::from_secs(
                                RETRY_DELAYS.get(attempt).copied().unwrap_or(0),
                            ))
                            .await;
                            continue 'retry;
                        }
                        return Err(err);
                    }
                };
                let choice = parsed.choices.first().ok_or("Empty LLM response")?;
                let msg = choice.message.clone().unwrap_or(ResponseMessage {
                    content: None,
                    reasoning_content: None,
                    tool_calls: None,
                });
                let text = msg.content.unwrap_or_default();
                let reasoning = msg.reasoning_content.unwrap_or_default();
                let tool_calls = msg.tool_calls.unwrap_or_default();
                let usage = parsed.usage.map(|u| TokenUsage {
                    prompt: u.prompt_tokens,
                    completion: u.completion_tokens,
                    cache_hit: u.prompt_cache_hit_tokens,
                    cache_miss: u.prompt_cache_miss_tokens,
                });
                if text.is_empty() && tool_calls.is_empty() {
                    return Err("Empty response (no text, no tool calls)".into());
                }
                return Ok((text, reasoning, tool_calls, usage));
            }

            // ── SSE stream parsing ──
            let mut full_text = String::new();
            let mut full_reasoning = String::new();
            // tool-call deltas keyed by index
            let mut tool_acc: std::collections::BTreeMap<
                u32,
                (Option<String>, Option<String>, Option<String>),
            > = std::collections::BTreeMap::new();
            let mut finish_reason: Option<String> = None;
            // usage arrives (with stream_options.include_usage) in the last chunk
            let mut usage: Option<TokenUsage> = None;

            let mut stream = resp.bytes_stream();
            let mut buffer = String::new();
            use futures_util::StreamExt;

            while let Some(chunk) = stream.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        // Stream broke mid-body (connection reset, decode error,
                        // proxy timeout). The request itself is idempotent —
                        // no tool has executed yet — so restarting it is safe.
                        // Always retry with backoff; only surface after retries
                        // are exhausted.
                        let err = format!("Stream error: {e}");
                        if attempt < RETRY_DELAYS.len() {
                            tokio::time::sleep(std::time::Duration::from_secs(
                                RETRY_DELAYS.get(attempt).copied().unwrap_or(0),
                            ))
                            .await;
                            continue 'retry;
                        }
                        return Err(err);
                    }
                };
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                // process complete lines
                while let Some(pos) = buffer.find('\n') {
                    let line: String = buffer.drain(..=pos).collect();
                    let line = line.trim();
                    if let Some(data) = line.strip_prefix("data: ") {
                        if data.trim() == "[DONE]" {
                            continue;
                        }
                        if let Ok(v) = serde_json::from_str::<Value>(data) {
                            if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                                usage = Some(TokenUsage {
                                    prompt: u
                                        .get("prompt_tokens")
                                        .and_then(|x| x.as_u64())
                                        .unwrap_or(0)
                                        as u32,
                                    completion: u
                                        .get("completion_tokens")
                                        .and_then(|x| x.as_u64())
                                        .unwrap_or(0)
                                        as u32,
                                    cache_hit: u
                                        .get("prompt_cache_hit_tokens")
                                        .and_then(|x| x.as_u64())
                                        .unwrap_or(0)
                                        as u32,
                                    cache_miss: u
                                        .get("prompt_cache_miss_tokens")
                                        .and_then(|x| x.as_u64())
                                        .unwrap_or(0)
                                        as u32,
                                });
                            }
                            if let Some(choices) = v.get("choices").and_then(|c| c.as_array()) {
                                if let Some(choice0) = choices.first() {
                                    if let Some(fr) =
                                        choice0.get("finish_reason").and_then(|f| f.as_str())
                                    {
                                        if !fr_eq(
                                            fr_is_null(finish_reason.as_deref()),
                                            fr_is_null(Some(fr)),
                                        ) && finish_reason.is_none()
                                        {
                                            finish_reason = Some(fr.to_string());
                                        }
                                    }
                                    if let Some(delta) = choice0.get("delta") {
                                        if let Some(content) =
                                            delta.get("content").and_then(|c| c.as_str())
                                        {
                                            if !content.is_empty() {
                                                full_text.push_str(content);
                                                if let Some(cb) = on_delta {
                                                    cb(&StreamDelta::Content {
                                                        text: content.to_string(),
                                                    });
                                                }
                                            }
                                        }
                                        // reasoning_content (DeepInfra/GLM/DeepSeek)
                                        // or reasoning (OpenAI-compatible providers)
                                        let reasoning = delta
                                            .get("reasoning_content")
                                            .and_then(|c| c.as_str())
                                            .or_else(|| {
                                                delta.get("reasoning").and_then(|c| c.as_str())
                                            });
                                        if let Some(r) = reasoning {
                                            if !r.is_empty() {
                                                full_reasoning.push_str(r);
                                                if let Some(cb) = on_delta {
                                                    cb(&StreamDelta::Reasoning {
                                                        text: r.to_string(),
                                                    });
                                                }
                                            }
                                        }
                                        if let Some(tcs) =
                                            delta.get("tool_calls").and_then(|t| t.as_array())
                                        {
                                            for tc in tcs {
                                                let idx = tc
                                                    .get("index")
                                                    .and_then(|i| i.as_u64())
                                                    .unwrap_or(0)
                                                    as u32;
                                                let e = tool_acc
                                                    .entry(idx)
                                                    .or_insert((None, None, None));
                                                if let Some(id) =
                                                    tc.get("id").and_then(|i| i.as_str())
                                                {
                                                    e.0 = Some(id.to_string());
                                                }
                                                if let Some(name) = tc
                                                    .pointer("/function/name")
                                                    .and_then(|n| n.as_str())
                                                {
                                                    let acc = e.1.get_or_insert_with(String::new);
                                                    acc.push_str(name);
                                                }
                                                if let Some(args) = tc
                                                    .pointer("/function/arguments")
                                                    .and_then(|a| a.as_str())
                                                {
                                                    let a = e.2.get_or_insert_with(String::new);
                                                    a.push_str(args);
                                                    // live progress to the UI: tool + growing arg size
                                                    if !args.is_empty() {
                                                        if let Some(cb) = on_delta {
                                                            cb(&StreamDelta::ToolArgs {
                                                                tool: e
                                                                    .1
                                                                    .clone()
                                                                    .unwrap_or_default(),
                                                                total_chars: a.len(),
                                                                preview: args_target(a)
                                                                    .unwrap_or_default(),
                                                            });
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            let tool_calls: Vec<ToolCall> = tool_acc
                .into_iter()
                .map(|(idx, (id, name, args))| ToolCall {
                    id: id.unwrap_or_else(|| format!("call_{idx}")),
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: name.unwrap_or_default(),
                        arguments: args.unwrap_or_default(),
                    },
                })
                .collect();

            if full_text.is_empty() && tool_calls.is_empty() {
                return Err("Empty stream (no text, no tool calls)".into());
            }
            // length-Truncation guard: cut-off tool args = invalid JSON that
            // would only fail later ("path is required") — fail HERE with the
            // true cause so the model/user sees the real problem.
            if finish_reason.as_deref() == Some("length") && !tool_calls.is_empty() {
                return Err(format!(
                    "Tool-call arguments were cut off at the {}-token output limit, so the \
                     arguments are incomplete. Do NOT retry the same call: write the file in \
                     smaller pieces — one `write_file` for the first part, then `patch` to \
                     extend it. (Raising llm.max_tokens only helps when this model allows \
                     more than {} output tokens.)",
                    self.max_tokens, self.max_tokens
                ));
            }
            let _ = &finish_reason;
            return Ok((full_text, full_reasoning, tool_calls, usage));
        }
        unreachable!("retry loop");
    }

    /// Build OpenAI-format tool definitions from the registry
    pub fn tool_definitions(&self, state: &AppState) -> Vec<Value> {
        state
            .tools
            .get_names()
            .iter()
            .filter_map(|name| {
                let meta = state.tools.get_meta(name)?;
                let schema = state.tools.get_schema(name)?;
                Some(serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        // No emoji prefix: costs only tokens, the model makes
                        // the choice via the description.
                        "description": meta.description,
                        "parameters": schema.raw().clone(),
                    }
                }))
            })
            .collect()
    }
}

fn fr_is_null(o: Option<&str>) -> bool {
    o.is_none()
}
fn fr_eq(a: bool, b: bool) -> bool {
    a == b
}

// ─── Offline mock provider (test-only) ────────────────────────────────────

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    OnceLock,
};

static MOCK_SCRIPT: OnceLock<Option<Value>> = OnceLock::new();
static MOCK_TURN: AtomicUsize = AtomicUsize::new(0);

/// Parsed script when RIJO_LLM_MOCK points at a readable JSON file.
fn mock_script() -> Option<&'static Value> {
    MOCK_SCRIPT
        .get_or_init(|| {
            let path = std::env::var("RIJO_LLM_MOCK").ok()?;
            if path.is_empty() {
                return None;
            }
            let raw = std::fs::read_to_string(&path).ok()?;
            serde_json::from_str::<Value>(&raw).ok()
        })
        .as_ref()
}

/// Serve the next scripted turn. Exhausted script → final empty answer,
/// which the executor treats as a normal end of turn.
fn mock_turn(
    script: &Value,
    on_delta: Option<StreamCallback<'_>>,
) -> Result<(String, String, Vec<ToolCall>, Option<TokenUsage>), String> {
    let idx = MOCK_TURN.fetch_add(1, Ordering::SeqCst);
    let turns = script.get("turns").and_then(|t| t.as_array());
    let turn = turns.and_then(|t| t.get(idx));

    let Some(turn) = turn else {
        // Script exhausted — the loop below only continues while tool_calls
        // come back, so an empty turn cleanly terminates the run.
        if let Some(cb) = on_delta {
            cb(&StreamDelta::Content {
                text: "[mock] script exhausted".into(),
            });
        }
        return Ok((
            "[mock] script exhausted".into(),
            String::new(),
            Vec::new(),
            None,
        ));
    };

    if let Some(r) = turn.get("reasoning").and_then(|v| v.as_str()) {
        if let Some(cb) = on_delta {
            cb(&StreamDelta::Reasoning {
                text: r.to_string(),
            });
        }
    }

    let text = turn
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if !text.is_empty() {
        if let Some(cb) = on_delta {
            cb(&StreamDelta::Content { text: text.clone() });
        }
    }

    let mut calls = Vec::new();
    if let Some(arr) = turn.get("tool_calls").and_then(|v| v.as_array()) {
        for (i, tc) in arr.iter().enumerate() {
            let name = tc.get("name").and_then(|v| v.as_str()).unwrap_or_default();
            let arguments = tc
                .get("arguments")
                .map(|a| match a {
                    // Fixtures may carry args as a JSON object (readable) or as
                    // a pre-encoded string (exact API shape).
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_else(|| "{}".into());
            if let Some(cb) = on_delta {
                cb(&StreamDelta::ToolArgs {
                    tool: name.to_string(),
                    total_chars: arguments.len(),
                    preview: args_target(&arguments).unwrap_or_default(),
                });
            }
            calls.push(ToolCall {
                id: format!("mock_call_{idx}_{i}"),
                call_type: "function".into(),
                function: FunctionCall {
                    name: name.to_string(),
                    arguments,
                },
            });
        }
    }
    Ok((text, String::new(), calls, None))
} // seconds

#[cfg(test)]
mod attach_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn image_paths_are_recognised_case_insensitively() {
        assert!(is_image_path("a/b/photo.PNG"));
        assert!(is_image_path("x.jpeg"));
        assert!(is_image_path("icon.webp"));
        assert!(!is_image_path("main.js"));
        assert!(!is_image_path("notes.md"));
    }

    #[test]
    fn a_local_image_becomes_a_data_url_and_a_url_passes_through() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("dot.png");
        std::fs::write(&p, b"\x89PNG\r\n").unwrap();
        let data_url = image_ref(p.to_str().unwrap()).unwrap();
        assert!(data_url.starts_with("data:image/png;base64,"), "{data_url}");
        assert_eq!(
            image_ref("https://example.com/a.png").unwrap(),
            "https://example.com/a.png"
        );
        // A missing file is an error, never a panic.
        assert!(image_ref("does-not-exist.png").is_err());
    }

    /// A local prefill is minutes on a laptop, a cloud call is not. The two must
    /// not share the cloud timeout: that is what made a local turn impossible.
    #[test]
    fn a_local_endpoint_gets_the_long_timeout() {
        assert_eq!(request_timeout("http://127.0.0.1:8080/v1").as_secs(), 900);
        assert_eq!(request_timeout("http://localhost:1234/v1").as_secs(), 900);
        assert_eq!(request_timeout("http://[::1]:8080/v1").as_secs(), 900);
        assert_eq!(
            request_timeout("https://api.deepseek.com/v1").as_secs(),
            180
        );
        assert_eq!(request_timeout("https://api.openai.com/v1").as_secs(), 180);
    }

    #[test]
    fn a_text_message_keeps_the_plain_string_shape() {
        let m = ChatMessage {
            role: "user".into(),
            content: Some("hello".into()),
            ..Default::default()
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["content"], serde_json::json!("hello"));
        assert!(v.get("reasoning_content").is_none(), "None stays out");
        assert!(v.get("tool_calls").is_none());
        assert!(v.get("parts").is_none(), "parts is not a wire field");
    }

    #[test]
    fn parts_turn_content_into_the_multimodal_array() {
        let m = ChatMessage {
            role: "user".into(),
            content: Some("what is this".into()),
            parts: Some(vec![
                json!({ "type": "text", "text": "what is this" }),
                json!({
                    "type": "image_url",
                    "image_url": { "url": "data:image/png;base64,AA" }
                }),
            ]),
            ..Default::default()
        };
        let v = serde_json::to_value(&m).unwrap();
        assert!(v["content"].is_array(), "{v}");
        assert_eq!(v["content"][0]["text"], "what is this");
        assert_eq!(
            v["content"][1]["image_url"]["url"],
            "data:image/png;base64,AA"
        );
        // Field order is explicit and stable — the prompt cache depends on it.
        let s = serde_json::to_string(&m).unwrap();
        assert!(
            s.find("\"role\"").unwrap() < s.find("\"content\"").unwrap(),
            "{s}"
        );
    }

    /// The whole request body, not just the message: this is what actually goes
    /// over the wire, and it is where the multimodal shape has to land.
    #[test]
    fn the_request_body_carries_the_parts_array() {
        let req = ChatRequest {
            model: "gpt-4o".into(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: Some("hi".into()),
                parts: Some(vec![
                    json!({ "type": "text", "text": "hi" }),
                    json!({
                        "type": "image_url",
                        "image_url": { "url": "data:image/png;base64,AA" }
                    }),
                ]),
                ..Default::default()
            }],
            temperature: None,
            max_tokens: None,
            stream: None,
            tools: None,
            thinking: None,
            reasoning_effort: None,
            stream_options: None,
        };
        let body: Value = serde_json::to_value(&req).unwrap();
        let content = &body["messages"][0]["content"];
        assert!(content.is_array(), "{body}");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,AA");
    }

    #[test]
    fn request_body_uses_per_turn_thinking_effort_without_changing_defaults() {
        let body = |model: &str, configured: &str, override_effort: Option<&str>| {
            let (thinking, reasoning_effort) =
                request_reasoning_fields(model, configured, override_effort);
            serde_json::to_value(ChatRequest {
                model: model.to_string(),
                messages: Vec::new(),
                temperature: None,
                max_tokens: None,
                stream: None,
                tools: None,
                thinking,
                reasoning_effort,
                stream_options: None,
            })
            .unwrap()
        };

        let min = body("deepseek-chat", "med", Some("min"));
        assert_eq!(min["reasoning_effort"], "low");
        assert_eq!(min["thinking"]["type"], "enabled");
        let max = body("deepseek-chat", "med", Some("max"));
        assert_eq!(max["reasoning_effort"], "max");
        let configured = body("deepseek-chat", "max", None);
        assert_eq!(configured["reasoning_effort"], "max");
        let other_provider = body("gpt-4o", "med", Some("max"));
        assert!(other_provider.get("reasoning_effort").is_none());
        assert!(other_provider.get("thinking").is_none());
    }

    #[test]
    fn a_text_file_loads_capped_and_a_missing_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("notes.md");
        std::fs::write(&p, "hello from disk").unwrap();
        let a = load_attachment(p.to_str().unwrap()).unwrap();
        assert!(!a.is_image());
        assert_eq!(a.text.as_deref(), Some("hello from disk"));
        assert!(!a.truncated);

        let big = dir.path().join("big.txt");
        std::fs::write(&big, "y".repeat(MAX_ATTACH_CHARS + 50)).unwrap();
        let a = load_attachment(big.to_str().unwrap()).unwrap();
        assert!(a.truncated, "the cap is marked, not silent");
        assert_eq!(a.text.unwrap().chars().count(), MAX_ATTACH_CHARS);

        assert!(load_attachment("nope.txt").is_err());
    }

    #[test]
    fn an_image_file_loads_as_a_data_url() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("shot.png");
        std::fs::write(&p, b"\x89PNG\r\n").unwrap();
        let a = load_attachment(p.to_str().unwrap()).unwrap();
        assert!(a.is_image());
        assert!(a.data_url.unwrap().starts_with("data:image/png;base64,"));
        assert!(a.text.is_none());
    }

    /// All capability assertions in ONE test: `RIJO_VISION` is process-global, so
    /// spreading them over parallel tests would be a race.
    #[test]
    fn vision_capability_is_a_heuristic_with_an_env_escape_hatch() {
        std::env::remove_var("RIJO_VISION");
        assert!(model_supports_vision("gpt-4o-mini"));
        assert!(model_supports_vision("claude-sonnet-4"));
        assert!(model_supports_vision("qwen-vl-max"));
        assert!(model_supports_vision("google/gemini-2.0-flash"));
        assert!(!model_supports_vision("deepseek-chat"));
        assert!(!model_supports_vision("deepseek-reasoner"));
        assert!(!model_supports_vision("llama-3.1-70b"));

        // The override wins in BOTH directions.
        std::env::set_var("RIJO_VISION", "off");
        assert!(!model_supports_vision("gpt-4o"));
        std::env::set_var("RIJO_VISION", "on");
        assert!(model_supports_vision("deepseek-chat"));
        std::env::remove_var("RIJO_VISION");
    }
}

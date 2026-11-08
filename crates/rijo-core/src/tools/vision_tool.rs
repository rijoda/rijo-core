//! Vision tool — analyze images via the configured LLM (vision-capable)
//!
//! Sends image as base64 data URL to the chat endpoint with a `image_url`
//! content part. Falls back to clear error when the model/provider rejects it.

use crate::tools::{tool_error, AsyncTool, JsonSchema, ToolMeta, ToolResult};
use crate::AppState;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

pub struct VisionAnalyze;

#[derive(serde::Deserialize)]
struct VisionPayload {
    image_url: String,
    #[serde(default)]
    question: String,
}

#[async_trait]
impl AsyncTool for VisionAnalyze {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "vision_analyze",
            description: "Analyse an image (URL or path) with the vision LLM.",
            emoji: "👁️",
            toolset: "core",
            is_async: true,
        }
    }

    fn schema(&self) -> &JsonSchema {
        &VISION_SCHEMA
    }

    async fn call_async(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        let payload: VisionPayload = match serde_json::from_value(args) {
            Ok(p) => p,
            Err(e) => return tool_error(format!("bad args: {e}")),
        };
        if payload.image_url.is_empty() {
            return tool_error("image_url is required");
        }
        let question = if payload.question.is_empty() {
            "Describe this image".to_string()
        } else {
            payload.question
        };

        // Local path → data URL, http(s) through. Encoding and request shape live
        // in `llm` so the TUI attachment path and this tool cannot drift apart —
        // and so the provider's session header travels with the request.
        let image_ref = match crate::llm::image_ref(&payload.image_url) {
            Ok(r) => r,
            Err(e) => return tool_error(e),
        };
        let client = crate::llm::LlmClient::new(&state.config.llm);
        match client.describe_image(&image_ref, &question).await {
            Ok(text) => json!({ "description": text, "image_url": payload.image_url }).to_string(),
            Err(e) => tool_error(e),
        }
    }
}

lazy_static::lazy_static! {
    static ref VISION_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "image_url": { "type": "string", "description": "Image URL or local file path" },
            "question": { "type": "string", "description": "Question (default: describe)" }
        },
        "required": ["image_url"]
    }));
}

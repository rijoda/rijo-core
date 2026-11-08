//! `clarify` — ask the user, loudly.
//!
//! In the TUI this BLOCKS: it sends `UiEvent::Ask`, the question overlay collects
//! the answers and the turn continues with them. That is what makes a question
//! interactive instead of a JSON blob.
//!
//! Headless (`--query`, `mcp serve`, tests) there is no UI to answer, so it
//! degrades to the earlier behaviour: the questions come back as data and the
//! answers arrive as the next user message. It never hangs either way — if the
//! TUI goes away, the oneshot sender is dropped and the fallback runs.

use crate::tools::{tool_error, JsonSchema, Tool, ToolMeta, ToolResult};
use crate::AppState;
use serde_json::{json, Value};
use std::sync::Arc;

/// Kept small on purpose: a question list is a decision, not a form. The model is
/// told to ask fewer, sharper questions.
const MAX_QUESTIONS: usize = 5;
const MAX_OPTIONS: usize = 6;

pub struct ClarifyTool;

impl Tool for ClarifyTool {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "clarify",
            description: "Ask the user for a decision. Shown as an interactive prompt in the TUI and BLOCKS until answered. Use it before multi-step work when a choice is genuinely missing; do not ask what you can decide yourself.",
            emoji: "❓",
            toolset: "core",
            is_async: false,
        }
    }

    fn schema(&self) -> &JsonSchema {
        &CLARIFY_SCHEMA
    }

    fn call(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        let questions = parse_questions(&args);
        if questions.is_empty() {
            return tool_error("questions must not be empty");
        }

        // A TUI is attached: ask for real and wait for the answers.
        let sender = state
            .ui_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(tx) = sender {
            let (resp_tx, resp_rx) = tokio::sync::oneshot::channel::<Vec<(String, String)>>();
            if tx
                .send(crate::ui_events::UiEvent::Ask {
                    questions: questions.clone(),
                    tx: Arc::new(resp_tx),
                })
                .is_ok()
            {
                // `blocking_recv` is correct here: this tool runs on the blocking
                // pool (spawn_blocking), not on the async runtime.
                if let Ok(answers) = resp_rx.blocking_recv() {
                    let answers: Vec<Value> = answers
                        .into_iter()
                        .map(|(id, answer)| json!({ "id": id, "answer": answer }))
                        .collect();
                    return json!({ "answers": answers }).to_string();
                }
            }
        }

        // No UI (or it went away): hand the questions back for the text path.
        json!({
            "needs_clarification": true,
            "questions": args.get("questions").cloned().unwrap_or(Value::Null),
            "hint": "No interactive UI is attached. Ask the user these questions and use their next message as the answer."
        })
        .to_string()
    }
}

/// Read the questions out of the call, capped and with derived ids so a caller
/// that sends none still works.
fn parse_questions(args: &Value) -> Vec<crate::ui_events::AskQuestion> {
    args.get("questions")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .take(MAX_QUESTIONS)
                .enumerate()
                .filter_map(|(i, q)| {
                    let question = q["question"].as_str().unwrap_or("").trim().to_string();
                    if question.is_empty() {
                        return None;
                    }
                    Some(crate::ui_events::AskQuestion {
                        id: q["id"]
                            .as_str()
                            .map(String::from)
                            .unwrap_or_else(|| format!("q{}", i + 1)),
                        question,
                        options: q["options"]
                            .as_array()
                            .map(|o| {
                                o.iter()
                                    .take(MAX_OPTIONS)
                                    .filter_map(|opt| {
                                        let label = opt["label"].as_str()?.trim();
                                        if label.is_empty() {
                                            return None;
                                        }
                                        Some(crate::ui_events::AskOption {
                                            label: label.to_string(),
                                            recommended: opt["recommended"]
                                                .as_bool()
                                                .unwrap_or(false),
                                        })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                        allow_free_text: q["allow_free_text"].as_bool().unwrap_or(true),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

lazy_static::lazy_static! {
    static ref CLARIFY_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "questions": {
                "type": "array",
                "description": "At most 5 questions; each a decision with a few options",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "description": "Short id, echoed back with the answer" },
                        "question": { "type": "string" },
                        "options": {
                            "type": "array",
                            "description": "Up to 6 options; the user can always answer freely too",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "label": { "type": "string" },
                                    "recommended": { "type": "boolean", "description": "Mark THE option you would pick" }
                                },
                                "required": ["label"]
                            }
                        },
                        "allow_free_text": { "type": "boolean", "description": "Offer an \"Anderer …\" free-text row (default true)" }
                    },
                    "required": ["question"]
                }
            }
        },
        "required": ["questions"]
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_derived_and_limits_enforced() {
        let args = json!({
            "questions": [
                { "question": "A?", "options": [ {"label": "x", "recommended": true}, {"label": "y"} ] },
                { "id": "keep", "question": "B?" },
                { "question": "  " }
            ]
        });
        let qs = parse_questions(&args);
        assert_eq!(qs.len(), 2, "the empty question is dropped");
        assert_eq!(qs[0].id, "q1");
        assert_eq!(qs[1].id, "keep");
        assert!(qs[0].options[0].recommended);
        assert!(!qs[0].options[1].recommended);
        // Free text is on unless switched off.
        assert!(qs[1].allow_free_text);
    }

    #[test]
    fn at_most_five_questions() {
        let list: Vec<Value> = (0..9)
            .map(|i| json!({"question": format!("q{i}")}))
            .collect();
        assert_eq!(
            parse_questions(&json!({"questions": list})).len(),
            MAX_QUESTIONS
        );
    }
}

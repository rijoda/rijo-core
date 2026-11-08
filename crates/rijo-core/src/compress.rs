//! Context compression — the pure planning core (chat-history compression plan).
//!
//! Three layers, cheapest first: structural pruning (`prune_context`, exists),
//! FTS5 recall (`context recall`, exists), and LLM summarization. The expensive
//! part is the model call; everything that decides **what** to summarize, **how**
//! to fold the answer back, and **when** to trigger lives here — pure and
//! unit-testable, with no model and no database.
//!
//! Incrementality is the point: after the first compression the history starts
//! with one summary message. [`plan`] treats that leading message as the
//! *previous summary* (to merge), not as content to re-summarize — so the next
//! run condenses only the messages above the watermark, O(new) instead of O(all).

use crate::llm::ChatMessage;

/// Marker that a history entry is a folded summary, not a real turn.
pub const SUMMARY_PREFIX: &str = "[history summary";

/// Is this entry a folded summary (a `system` message we wrote ourselves)?
pub fn is_summary_message(m: &ChatMessage) -> bool {
    m.role == "system"
        && m.content
            .as_deref()
            .is_some_and(|c| c.starts_with(SUMMARY_PREFIX))
}

fn text_of(m: &ChatMessage) -> String {
    m.content.clone().unwrap_or_default()
}

/// What a compression would do.
#[derive(Debug, Clone)]
pub struct Plan {
    /// A previous summary to merge into (the leading summary message, if any).
    pub previous: Option<String>,
    /// The messages the model should condense.
    pub to_summarize: Vec<ChatMessage>,
    /// The messages that stay verbatim (the newest `keep_recent`).
    pub kept: Vec<ChatMessage>,
}

impl Plan {
    /// Nothing to do (the history is already short enough).
    pub fn is_empty(&self) -> bool {
        self.to_summarize.is_empty()
    }
}

/// Split a history into (summarize, keep). A leading summary message is the
/// previous summary, not content to re-summarize — that is what makes the second
/// run incremental.
pub fn plan(history: &[ChatMessage], keep_recent: usize) -> Plan {
    let (previous, body) = match history.split_first() {
        Some((first, rest)) if is_summary_message(first) => (first.content.clone(), rest),
        _ => (None, history),
    };
    if body.len() <= keep_recent {
        return Plan {
            previous,
            to_summarize: Vec::new(),
            kept: body.to_vec(),
        };
    }
    let cut = body.len() - keep_recent;
    Plan {
        previous,
        to_summarize: body[..cut].to_vec(),
        kept: body[cut..].to_vec(),
    }
}

/// A plain history message (no tool calls) — the shape summaries are built from.
pub fn message(role: &str, content: &str) -> ChatMessage {
    ChatMessage {
        role: role.to_string(),
        content: Some(content.to_string()),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
        parts: None,
    }
}

/// The prompt that condenses `to_summarize`, merging `previous` when present.
pub fn summary_prompt(
    previous: Option<&str>,
    to_summarize: &[ChatMessage],
    max_tokens: u32,
) -> Vec<ChatMessage> {
    let mut turns = String::new();
    for m in to_summarize {
        let role = match m.role.as_str() {
            "user" => "USER",
            "assistant" => "ASSISTANT",
            "tool" => "TOOL",
            "system" => "SYSTEM",
            other => other,
        };
        turns.push_str(&format!("{role}: {}\n", text_of(m)));
    }
    let system = "You compress an agent's conversation history. Be factual and terse. \
        Decisions, file paths, identifiers, commands and open questions stay VERBATIM — \
        never invent or rename anything. Drop pleasantries and repetition. \
        Output plain text, no markdown headings.";
    let mut user = String::new();
    if let Some(prev) = previous {
        user.push_str("Existing summary to extend (merge, do not repeat verbatim):\n");
        user.push_str(prev);
        user.push_str("\n\n");
    }
    user.push_str(&format!(
        "Newer conversation to fold in (at most ~{max_tokens} tokens of output):\n{turns}"
    ));
    vec![message("system", system), message("user", &user)]
}

/// The model's answer as a stored summary: trimmed and capped to the token
/// budget (≈4 chars/token, the project-wide convention) so a runaway reply
/// cannot blow the block size.
pub fn parse_summary(answer: &str, max_tokens: u32) -> String {
    let cap = (max_tokens as usize).saturating_mul(4);
    let trimmed = answer.trim();
    if trimmed.chars().count() <= cap {
        return trimmed.to_string();
    }
    trimmed.chars().take(cap).collect()
}

/// Call the model to condense `to_summarize`, merging `previous` when present.
///
/// The answer is trimmed and capped with [`parse_summary`]; an empty answer is an
/// error, so a bad call cannot quietly wipe the history.
pub async fn summarize(
    llm: &crate::llm::LlmClient,
    previous: Option<&str>,
    to_summarize: &[ChatMessage],
    max_tokens: u32,
) -> Result<String, String> {
    if to_summarize.is_empty() {
        return Err("nothing to summarize".to_string());
    }
    let prompt = summary_prompt(previous, to_summarize, max_tokens);
    let (text, _reasoning, _tool_calls, _usage) = llm.chat(&prompt, &[]).await?;
    let summary = parse_summary(&text, max_tokens);
    if summary.is_empty() {
        return Err("the model returned an empty summary".to_string());
    }
    Ok(summary)
}

/// The single folded history entry: `[history summary up to message N] …`.
pub fn summary_message(watermark: i64, summary: &str) -> ChatMessage {
    message(
        "system",
        &format!(
            "{SUMMARY_PREFIX} up to message {watermark}]\n{}",
            summary.trim()
        ),
    )
}

/// Should the executor compress before the next request?
///
/// True only at/above the window percentage and after the minimum turn gap —
/// both guards exist so a compression does not invalidate the provider prompt
/// cache on every turn.
pub fn should_compress(
    prompt_tokens: u64,
    context_window: u64,
    at_pct: u32,
    turns_since_last: u32,
    min_turns_between: u32,
) -> bool {
    if context_window == 0 || turns_since_last < min_turns_between {
        return false;
    }
    prompt_tokens >= context_window.saturating_mul(at_pct as u64) / 100
}

/// Is the plan worth a model call at all? Guards against churning the cache for a
/// saving of a few tokens.
pub fn worth_it(plan: &Plan, min_saving_tokens: u32) -> bool {
    let chars: usize = plan
        .to_summarize
        .iter()
        .map(|m| text_of(m).chars().count())
        .sum();
    (chars as u64) / 4 >= min_saving_tokens as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msgs(n: usize) -> Vec<ChatMessage> {
        (0..n)
            .map(|i| {
                message(
                    if i % 2 == 0 { "user" } else { "assistant" },
                    &format!("m{i}"),
                )
            })
            .collect()
    }

    #[test]
    fn a_short_history_is_not_compressed() {
        let p = plan(&msgs(5), 12);
        assert!(p.is_empty(), "nothing to summarize");
        assert_eq!(p.kept.len(), 5);
        assert!(p.previous.is_none());
    }

    #[test]
    fn everything_but_the_recent_tail_is_summarized() {
        let p = plan(&msgs(30), 12);
        assert_eq!(p.to_summarize.len(), 18);
        assert_eq!(p.kept.len(), 12);
        // The kept tail is the NEWEST 12, in order.
        assert_eq!(p.kept.first().unwrap().content.as_deref(), Some("m18"));
        assert_eq!(p.kept.last().unwrap().content.as_deref(), Some("m29"));
    }

    #[test]
    fn a_leading_summary_is_the_previous_summary_not_content() {
        let mut history = vec![summary_message(42, "older facts")];
        history.extend(msgs(30));
        let p = plan(&history, 12);
        assert_eq!(
            p.previous.as_deref(),
            Some("[history summary up to message 42]\nolder facts")
        );
        // Only the 18 non-recent REAL messages are re-summarized — the summary
        // itself is merged, never fed back as content (incremental).
        assert_eq!(p.to_summarize.len(), 18);
        assert!(p.to_summarize.iter().all(|m| !is_summary_message(m)));
    }

    #[test]
    fn the_prompt_carries_the_previous_summary_when_present() {
        let prompt = summary_prompt(Some("prev facts"), &msgs(2), 1200);
        assert_eq!(prompt.len(), 2);
        let user = prompt[1].content.clone().unwrap();
        assert!(user.contains("prev facts"), "{user}");
        assert!(user.contains("USER: m0"), "{user}");
        assert!(user.contains("ASSISTANT: m1"), "{user}");
    }

    #[test]
    fn parsing_caps_the_block_to_the_token_budget() {
        let long = "x".repeat(100);
        // 5 tokens ≈ 20 chars.
        assert_eq!(parse_summary(&long, 5).chars().count(), 20);
        assert_eq!(parse_summary("  short  ", 1200), "short");
    }

    #[test]
    fn the_folded_message_carries_the_watermark_and_is_recognised() {
        let m = summary_message(42, "facts");
        assert!(is_summary_message(&m));
        assert_eq!(
            m.content.as_deref(),
            Some("[history summary up to message 42]\nfacts")
        );
    }

    #[test]
    fn compression_triggers_only_at_the_threshold_and_after_the_gap() {
        // 80 % of 100k = 80k.
        assert!(!should_compress(79_000, 100_000, 80, 5, 5));
        assert!(should_compress(80_000, 100_000, 80, 5, 5));
        // Too soon after the last compression — keep the cache stable.
        assert!(!should_compress(99_000, 100_000, 80, 4, 5));
        // Unknown window: never guess.
        assert!(!should_compress(99_000, 0, 80, 99, 5));
    }

    #[test]
    fn a_tiny_saving_is_not_worth_a_model_call() {
        // 18 old messages × 500 chars ≈ 2250 tokens — worth folding.
        let long: Vec<ChatMessage> = (0..30).map(|_| message("user", &"x".repeat(500))).collect();
        assert!(worth_it(&plan(&long, 12), 2000));
        // One tiny message to fold is not worth a model call.
        assert!(!worth_it(&plan(&msgs(13), 12), 2000));
    }
}

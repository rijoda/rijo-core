//! Model facts that Rijo Code has to know without asking: currently the context
//! window, which the status line needs for its gauge.
//!
//! Why a table and not an API call: the OpenAI-compatible `/models` endpoint
//! lists ids, not limits, so there is nothing to ask. The table is best-effort
//! from vendor documentation, `llm.context_window` in the config overrides it,
//! and an unknown model stays unknown (the gauge then shows the prompt size
//! without a percentage — a made-up denominator would be a lie).

/// Context window of a known model family, looked up by name substring.
///
/// Matching is case-insensitive and ordered: the first hit wins, so more
/// specific entries come first ("deepseek-chat" before "deepseek").
pub fn known_context_window(model: &str) -> Option<u64> {
    let m = model.to_ascii_lowercase();
    // (substring, window) — keep the list short and only with numbers that are
    // documented; anything unverified stays out.
    const TABLE: &[(&str, u64)] = &[
        // OpenCode Zen / Go free tier. The /models endpoint lists ids without
        // limits, so these are the documented/assumed windows — deliberately
        // conservative: a wrong number would only distort the gauge, never the
        // request.
        ("space-bunny-free", 1_000_000),
        ("muse-spark", 128_000),
        ("nemotron-3", 128_000),
        ("big-pickle", 128_000),
        ("mimo-v2", 128_000),
        ("ling-3", 128_000),
        ("hy3", 128_000),
        ("hy4", 128_000),
        ("glm-5", 200_000),
        ("kimi-k2", 256_000),
        ("grok-4", 256_000),
        ("gpt-5", 400_000),
        // DeepSeek: V4.x models (deepseek-flash, deepseek-v4-pro) list 1M, the
        // V3 generation (deepseek-chat / deepseek-reasoner) lists 128K.
        ("deepseek-flash", 1_000_000),
        ("deepseek-v4", 1_000_000),
        ("deepseek-chat", 128_000),
        ("deepseek-reasoner", 128_000),
        ("deepseek-v3", 128_000),
        ("deepseek", 128_000),
        // OpenAI
        ("gpt-4.1", 1_047_576),
        ("gpt-4o", 128_000),
        ("gpt-4-turbo", 128_000),
        ("gpt-4", 8_192),
        ("gpt-3.5", 16_385),
        ("o1", 200_000),
        ("o3", 200_000),
        ("o4", 200_000),
        // Anthropic
        ("claude", 200_000),
        // Google
        ("gemini-2", 1_000_000),
        ("gemini-1.5", 1_000_000),
        // Open weights
        ("llama-4", 131_072),
        ("llama-3", 131_072),
        ("qwen", 131_072),
        ("mistral", 32_768),
        ("mixtral", 32_768),
        // Local models Rijo Code runs itself (llama.cpp). gpt-oss and the
        // Qwen3 family natively support 131072; the status-line gauge must know
        // the number, otherwise it shows the prompt size without a percentage.
        ("gpt-oss", 131_072),
        ("nanbeige", 262_144),
    ];
    TABLE
        .iter()
        .find(|(needle, _)| m.contains(needle))
        .map(|(_, window)| *window)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_models_resolve_to_their_documented_window() {
        assert_eq!(known_context_window("deepseek-flash"), Some(1_000_000));
        assert_eq!(known_context_window("deepseek-chat"), Some(128_000));
        assert_eq!(known_context_window("deepseek-reasoner"), Some(128_000));
        assert_eq!(known_context_window("gpt-4o-mini"), Some(128_000));
        assert_eq!(known_context_window("gpt-4o"), Some(128_000));
        assert_eq!(known_context_window("claude-sonnet-4"), Some(200_000));
        assert_eq!(known_context_window("gemini-2.0-flash"), Some(1_000_000));
        assert_eq!(known_context_window("llama-3.1-70b"), Some(131_072));
        // Local models Rijo Code may run itself.
        assert_eq!(
            known_context_window("gpt-oss-20b-Q4_K_M.gguf"),
            Some(131_072)
        );
        assert_eq!(known_context_window("Qwen3-30B-A3B"), Some(131_072));
    }

    #[test]
    fn the_specific_entry_wins_over_the_family_default() {
        // "deepseek-v4" must not be caught by the plain "deepseek" fallback.
        assert_eq!(known_context_window("deepseek-v4-pro"), Some(1_000_000));
        // …and V3 names must not be caught by the V4 entry.
        assert_eq!(known_context_window("deepseek-chat"), Some(128_000));
    }

    #[test]
    fn unknown_models_stay_unknown() {
        assert_eq!(known_context_window("my-local-finetune"), None);
        assert_eq!(known_context_window(""), None);
    }
}

#[cfg(test)]
mod resolution_tests {
    use super::known_context_window;
    use crate::config::LlmConfig;

    fn cfg(model: &str, explicit: u64) -> LlmConfig {
        // No Default impl on LlmConfig (it carries a base_url/api_key pair), so
        // take the project default and adjust.
        let mut llm = crate::config::Config::default().llm;
        llm.model = model.into();
        llm.context_window = explicit;
        llm
    }

    #[test]
    fn opencode_models_have_a_window() {
        assert_eq!(known_context_window("space-bunny-free"), Some(1_000_000));
        assert_eq!(known_context_window("deepseek-v4-pro"), Some(1_000_000));
        assert_eq!(known_context_window("kimi-k2.7-code"), Some(256_000));
        assert_eq!(known_context_window("glm-5.3"), Some(200_000));
    }

    #[test]
    fn an_explicit_window_wins_over_the_table() {
        assert_eq!(
            cfg("deepseek-flash", 64_000).effective_context_window(),
            Some(64_000)
        );
        assert_eq!(
            cfg("my-local-finetune", 32_000).effective_context_window(),
            Some(32_000)
        );
    }

    #[test]
    fn zero_means_detect_from_the_model_name() {
        assert_eq!(
            cfg("deepseek-flash", 0).effective_context_window(),
            Some(1_000_000)
        );
        assert_eq!(
            cfg("deepseek-chat", 0).effective_context_window(),
            Some(128_000)
        );
        // Unknown model + no override: still unknown, so the gauge shows no
        // percentage instead of a guessed one.
        assert_eq!(cfg("my-local-finetune", 0).effective_context_window(), None);
    }
}

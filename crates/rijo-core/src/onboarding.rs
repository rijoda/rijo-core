//! Onboarding — first-run setup wizard (provider + API key)
//!
//! Triggered when rijo starts interactively with no valid provider config.
//! Validates the key live against the provider endpoint, writes
//! ~/.rijo/config.yaml with chmod 600. Bypass: RIJO_SKIP_ONBOARD=1 or
//! `rijo --query`/flags (non-interactive paths never onboard).

use crate::config::Config;
use std::io::{BufRead, Write};

/// Wizard needed? (no valid api_key and not explicitly bypassed)
pub fn needed(config: &Config) -> bool {
    if std::env::var("RIJO_SKIP_ONBOARD").as_deref() == Ok("1") {
        return false;
    }
    config.llm.api_key.trim().is_empty()
}

/// One provider the first-run surface can offer.
///
/// Derived from the templates in `providers.rs`, so the desktop view and the CLI
/// wizard cannot disagree about a URL, a default model or a key variable.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Choice {
    pub id: &'static str,
    pub label: &'static str,
    pub base_url: &'static str,
    pub default_model: &'static str,
    pub api_key_env: &'static str,
    pub needs_key: bool,
    pub note: &'static str,
}

/// The menu, in the order a first-run should present it.
pub fn choices() -> Vec<Choice> {
    let tpl = |name: &str| {
        crate::providers::by_name(name).unwrap_or_else(|| panic!("no template for {name}"))
    };
    let (deepseek, opencode, deepinfra, local) = (
        tpl("deepseek"),
        tpl("opencode"),
        tpl("deepinfra"),
        tpl("local"),
    );
    vec![
        Choice {
            id: deepseek.name,
            label: deepseek.label,
            base_url: deepseek.base_url,
            default_model: deepseek.default_model,
            api_key_env: deepseek.api_key_env,
            needs_key: true,
            note: "Direct API, pay per token",
        },
        Choice {
            id: opencode.name,
            label: opencode.label,
            base_url: opencode.base_url,
            default_model: opencode.default_model,
            api_key_env: opencode.api_key_env,
            needs_key: true,
            note: "Subscription, coding-optimised",
        },
        Choice {
            id: deepinfra.name,
            label: deepinfra.label,
            base_url: deepinfra.base_url,
            default_model: deepinfra.default_model,
            api_key_env: deepinfra.api_key_env,
            needs_key: true,
            note: "Open models, pay per use",
        },
        Choice {
            id: local.name,
            label: local.label,
            base_url: local.base_url,
            default_model: local.default_model,
            api_key_env: local.api_key_env,
            needs_key: false,
            note: "llama-server on this machine, no key",
        },
        Choice {
            id: "custom",
            label: "Custom endpoint",
            base_url: "https://api.openai.com/v1",
            default_model: "gpt-4o-mini",
            api_key_env: "RIJO_API_KEY",
            needs_key: true,
            note: "Any OpenAI-compatible API",
        },
    ]
}

/// Writes a provider choice and returns the resulting config.
///
/// Validation is live for anything that is not a local endpoint: the wizard's
/// promise ("we check the key") must hold in the desktop too. A local base URL is
/// accepted without a key and without a call, because the server may simply not be
/// running yet — saving the address is still the useful step.
///
/// The rest of the config is preserved; only `llm` is replaced.
pub fn apply(id: &str, base_url: &str, model: &str, api_key: &str) -> Result<Config, String> {
    let choice = choices()
        .into_iter()
        .find(|choice| choice.id == id)
        .ok_or_else(|| format!("Unknown provider '{id}'."))?;
    let tpl = crate::providers::by_name(choice.id);

    let base = if base_url.trim().is_empty() {
        choice.base_url.to_string()
    } else {
        base_url.trim().trim_end_matches('/').to_string()
    };
    let model = if model.trim().is_empty() {
        choice.default_model.to_string()
    } else {
        model.trim().to_string()
    };
    let local = crate::local_server::is_local_base(&base);

    let mut key = api_key.trim().to_string();
    if key.is_empty() {
        if let Some(tpl) = &tpl {
            key = crate::providers::resolve_token(tpl, "").unwrap_or_default();
        }
    }
    if key.is_empty() && choice.needs_key && !local {
        return Err(format!("{} needs an API key.", choice.label));
    }
    if !local {
        validate_key(&base, &model, &key)?;
    }

    let mut cfg = Config::load(None);
    let mut llm = cfg.llm.clone();
    llm.base_url = base;
    llm.model = model;
    llm.api_key = key;
    llm.session_header = tpl
        .as_ref()
        .map(|t| t.session_header.to_string())
        .unwrap_or_default();
    llm.extra_headers = tpl
        .as_ref()
        .map(|t| t.extra_headers.iter().map(|s| s.to_string()).collect())
        .unwrap_or_default();
    // A different model means the old window says nothing about the new one.
    llm.context_window = 0;
    cfg.llm = llm;

    if !write_config(&cfg) {
        return Err(format!(
            "Could not write {}.\nIs the directory writable?",
            Config::default_path().display()
        ));
    }
    Ok(cfg)
}

pub fn run() -> Option<Config> {
    println!("Rijo Code CLI — native-speed coding agent (Rust), by Rijoda Labs");
    println!("First-time setup. At most three questions, and none if a key is already set.\n");

    // 1. Where to talk to. If a provider's key is already available — its own
    //    environment variable, or its CLI's credential file — nothing is asked.
    //    That is the shortest correct onboarding.
    //
    //    Endpoint, default model and the headers an API insists on come from the
    //    templates (providers.rs), never from a copy here: a bare OpenCode base
    //    URL without its session header answers HTTP 400 MissingSessionID, and
    //    the wizard must not be able to write such a config.
    let (tpl, base_url, default_model, api_key_env, env_key) = match detect_provider() {
        Some((t, key)) => {
            println!("Found a key for {} (${}).", t.label, t.api_key_env);
            println!("(--provider <name> picks another, --list-providers shows all)\n");
            (
                Some(t.clone()),
                t.base_url.to_string(),
                t.default_model.to_string(),
                t.api_key_env,
                Some(key),
            )
        }
        None => pick_provider()?,
    };

    // 2. Model. (A "Workspace" question used to sit here; its answer was thrown
    //    away — the workspace is the directory rijo runs in, resolved at start.)
    let model_in = prompt(&format!("Model [Enter = {default_model}]: "));
    let model = if model_in.is_empty() {
        default_model
    } else {
        model_in
    };

    // 3. Key + a live check against the endpoint with THAT model — validating a
    //    different model would test something the user never runs.
    let label = if env_key.is_some() {
        format!("API key [Enter = ${api_key_env}]: ")
    } else {
        "API key: ".to_string()
    };
    let mut api_key = String::new();
    for attempt in 1..=3 {
        let typed = prompt(&label);
        if !typed.trim().is_empty() {
            api_key = typed;
        } else if let Some(env) = &env_key {
            api_key = env.clone();
        }
        if api_key.trim().is_empty() {
            println!("  The key must not be empty.");
            continue;
        }
        print!("Validating ... ");
        let _ = std::io::stdout().flush();
        match validate_key(&base_url, &model, api_key.trim()) {
            Ok(()) => {
                println!("ok");
                break;
            }
            Err(e) => {
                println!("failed: {e}");
                if attempt == 3 {
                    println!("Aborted after 3 failed attempts. (RIJO_SKIP_ONBOARD=1 skips this)");
                    return None;
                }
            }
        }
    }
    if api_key.trim().is_empty() {
        return None;
    }

    finish(tpl.as_ref(), &base_url, &model, &api_key)
}

/// A provider whose key is already available — so nothing has to be asked.
///
/// `resolve_token` with an empty configured key is exactly the check: it looks at
/// the provider's own environment variable, then its CLI's credential file.
fn detect_provider() -> Option<(crate::providers::ProviderTemplate, String)> {
    crate::providers::all()
        .into_iter()
        .find_map(|tpl| crate::providers::resolve_token(&tpl, "").map(|key| (tpl, key)))
}

/// The menu, when nothing was detected.
///
/// Returns `(template, base_url, default_model, key variable, key already
/// available)`. Only DeepSeek and OpenCode have templates; the other two are
/// plain endpoints, so they carry no headers.
type ProviderChoice = (
    Option<crate::providers::ProviderTemplate>,
    String,
    String,
    &'static str,
    Option<String>,
);

fn pick_provider() -> Option<ProviderChoice> {
    let from = |tpl: crate::providers::ProviderTemplate| {
        let env_key = std::env::var(tpl.api_key_env)
            .ok()
            .filter(|k| !k.trim().is_empty());

        (
            Some(tpl.clone()),
            tpl.base_url.to_string(),
            tpl.default_model.to_string(),
            tpl.api_key_env,
            env_key,
        )
    };

    println!("Provider:");
    println!("  [1] deepseek    - the direct API, OpenAI-compatible  (default)");
    println!("  [2] opencode-go - subscription, coding-optimised");
    println!("  [3] deepinfra   - pay-per-use, 100+ open models");
    println!("  [4] custom      - your own OpenAI-compatible base_url");
    let choice = prompt("Choice [1]: ");

    let env_of = |name: &str| std::env::var(name).ok().filter(|k| !k.trim().is_empty());

    let chosen = match choice.trim() {
        "2" => Some(from(crate::providers::by_name("opencode")?)),
        "3" => Some(from(crate::providers::deepinfra())),
        "4" => {
            let base = prompt("Base URL [Enter = https://api.openai.com/v1]: ");
            let base = if base.trim().is_empty() {
                "https://api.openai.com/v1".to_string()
            } else {
                base.trim().to_string()
            };
            Some((
                None,
                base,
                "gpt-4o-mini".to_string(),
                "RIJO_API_KEY",
                env_of("RIJO_API_KEY"),
            ))
        }
        _ => Some(from(crate::providers::by_name("deepseek")?)),
    };

    // Say what was chosen. The menu path is otherwise silent about it, and the
    // auto-detect path already printed the same thing from the key side.
    if let Some((tpl, base, ..)) = &chosen {
        match tpl {
            Some(t) => println!("Using {}  ({base})", t.label),
            None => println!("Using {base}"),
        }
    }
    chosen
}

/// The LLM section the wizard writes, as a pure function.
///
/// Split out so the part that matters can be tested without a terminal or a
/// network call: the headers an API insists on come from the template. A bare
/// base URL is not enough for OpenCode — without `x-opencode-session` it answers
/// HTTP 400 MissingSessionID, and the wizard used to write exactly that.
fn llm_config(
    tpl: Option<&crate::providers::ProviderTemplate>,
    base_url: &str,
    model: &str,
    api_key: &str,
) -> crate::config::LlmConfig {
    crate::config::LlmConfig {
        base_url: base_url.to_string(),
        api_key: api_key.to_string(),
        model: model.to_string(),
        max_tokens: 4096,
        temperature: 0.7,
        thinking_effort: "med".into(),
        context_window: 0,
        session_header: tpl
            .map(|t| t.session_header.to_string())
            .unwrap_or_default(),
        extra_headers: tpl
            .map(|t| t.extra_headers.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default(),
    }
}

fn finish(
    tpl: Option<&crate::providers::ProviderTemplate>,
    base_url: &str,
    model: &str,
    api_key: &str,
) -> Option<Config> {
    let cfg = Config {
        llm: llm_config(tpl, base_url, model, api_key),
        ..Config::default()
    };
    if write_config(&cfg) {
        println!("\nConfiguration saved: ~/.rijo/config.yaml (chmod 600)");
        println!("Ready: {model}  @ {base_url}");
        println!("Try:   rijo --query \"answer with exactly one word: ok\"");
        Some(cfg)
    } else {
        println!("Could not write the configuration.");
        None
    }
}

fn prompt(label: &str) -> String {
    print!("{label}");
    let _ = std::io::stdout().flush();
    let mut buf = String::new();
    let _ = std::io::stdin().lock().read_line(&mut buf);
    buf.trim().to_string()
}

/// Live key validation: minimal chat call against the endpoint.
///
/// Runs on its own thread, because it uses a **blocking** client while `main` is
/// an async runtime: creating or dropping a blocking reqwest client inside a tokio
/// context panics with "Cannot drop a runtime in a context where blocking is not
/// allowed". Measured on the first real run — the onboarding's headline promise
/// ("validate the API key") crashed instead of validating, and nothing in the test
/// suite covered it because it needs a network call.
fn validate_key(base_url: &str, model: &str, key: &str) -> Result<(), String> {
    let (base_url, model, key) = (base_url.to_string(), model.to_string(), key.to_string());
    std::thread::spawn(move || validate_key_blocking(&base_url, &model, &key))
        .join()
        .unwrap_or_else(|_| Err("the validation thread panicked".to_string()))
}

fn validate_key_blocking(base_url: &str, model: &str, key: &str) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;

    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    // Validate with the model the user will actually run; an empty one falls back
    // to the cheapest guess so the check still says something about the KEY.
    let model = if model.trim().is_empty() {
        "gpt-4o-mini"
    } else {
        model.trim()
    };

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {key}"))
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "say ok"}],
            "max_tokens": 5,
        }))
        .send()
        .map_err(|e| format!("network: {e}"))?;

    if resp.status().is_success() {
        Ok(())
    } else {
        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            Err(format!("[{status}] key rejected"))
        } else {
            let snippet: String = resp.text().unwrap_or_default().chars().take(120).collect();
            Err(format!("[{status}] {snippet}"))
        }
    }
}

fn write_config(cfg: &Config) -> bool {
    let path = Config::default_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(yaml) = serde_yaml::to_string(cfg) else {
        return false;
    };
    if std::fs::write(&path, yaml).is_err() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialized: the detection reads process-wide environment variables.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The point of the simpler onboarding: a key that is already there means no
    /// questions at all, because the provider question only exists to find one.
    #[test]
    fn a_key_in_the_environment_is_detected_without_asking() {
        let _guard = env_lock();

        // An empty value counts as unset — it must not be reported as a key.
        // (assert_ne, not is_none: a machine with OpenCode's credential file
        // legitimately detects OpenCode here.)
        std::env::set_var("DEEPSEEK_API_KEY", "   ");
        assert_ne!(
            detect_provider().map(|(t, _)| t.name),
            Some("deepseek"),
            "a whitespace value is not a key"
        );

        std::env::set_var("DEEPSEEK_API_KEY", "ds-test");
        let (tpl, key) = detect_provider().expect("DeepSeek must be detected");
        assert_eq!(tpl.name, "deepseek");
        assert_eq!(key, "ds-test");
        // The detected template is what carries the endpoint and the headers.
        assert_eq!(tpl.base_url, "https://api.deepseek.com/v1");

        std::env::remove_var("DEEPSEEK_API_KEY");
    }

    /// The wizard must not be able to write a config the API rejects: choosing
    /// OpenCode in the menu has to carry `x-opencode-session`, or the first turn
    /// answers HTTP 400 `MissingSessionID`.
    #[test]
    fn the_wizard_writes_the_headers_the_api_insists_on() {
        let oc = crate::providers::by_name("opencode").expect("opencode template");
        let llm = llm_config(Some(&oc), oc.base_url, "space-bunny-free", "k");
        assert_eq!(llm.session_header, "x-opencode-session");
        assert!(
            !llm.extra_headers.is_empty(),
            "the User-Agent must travel too"
        );

        // DeepSeek needs neither, and a custom endpoint has no template at all.
        let ds = crate::providers::by_name("deepseek").expect("deepseek template");
        let llm = llm_config(Some(&ds), ds.base_url, "deepseek-chat", "k");
        assert!(llm.session_header.is_empty());
        assert!(llm.extra_headers.is_empty());

        let custom = llm_config(None, "https://api.example.com/v1", "m", "k");
        assert!(custom.session_header.is_empty());
        assert!(custom.extra_headers.is_empty());
        assert_eq!(custom.base_url, "https://api.example.com/v1");
    }

    /// The desktop's first-run list comes from the templates — a URL or default
    /// model that lives only in the view would be the drift this prevents.
    #[test]
    fn the_first_run_choices_are_derived_from_the_templates() {
        let choices = choices();
        let ids: Vec<_> = choices.iter().map(|c| c.id).collect();
        assert_eq!(
            ids,
            vec!["deepseek", "opencode", "deepinfra", "local", "custom"]
        );

        for choice in &choices {
            assert!(!choice.label.is_empty(), "{} has no label", choice.id);
            assert!(!choice.base_url.is_empty(), "{} has no base_url", choice.id);
            assert!(
                !choice.default_model.is_empty(),
                "{} has no model",
                choice.id
            );
        }

        let deepseek = crate::providers::deepseek();
        let listed = choices
            .iter()
            .find(|c| c.id == "deepseek")
            .expect("deepseek");
        assert_eq!(listed.base_url, deepseek.base_url);
        assert_eq!(listed.default_model, deepseek.default_model);
        assert_eq!(listed.api_key_env, deepseek.api_key_env);

        // A local endpoint is the one choice that must not demand a key.
        let local = choices.iter().find(|c| c.id == "local").expect("local");
        assert!(!local.needs_key);
        assert!(!crate::local_server::is_local_base(deepseek.base_url));
    }

    /// Both failures must happen before any network call, so this test is offline.
    #[test]
    fn apply_refuses_an_unknown_provider_and_a_missing_key() {
        assert!(apply("nope", "", "", "k")
            .unwrap_err()
            .contains("Unknown provider"));

        let _guard = env_lock();
        std::env::remove_var("DEEPSEEK_API_KEY");
        let err = apply("deepseek", "", "", "").unwrap_err();
        assert!(err.contains("needs an API key"), "{err}");
    }
}

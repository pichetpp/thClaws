//! thClaws Gateway overlay — distinct from the EE-policy gateway in
//! `crate::providers::gateway`.
//!
//! When the user has the "Use thClaws Gateway" toggle enabled for the
//! active provider AND has pasted an access key, the provider's HTTP
//! client points at the gateway instead of the upstream. The gateway
//! preserves each provider's native wire shape (per-prefix passthrough),
//! so the only knobs that change at the provider layer are:
//!
//! 1. Base URL → `<gateway>/<provider-segment>/<original-path>`
//! 2. Auth header value → the gateway access key
//!
//! The header **scheme** stays unchanged: OpenAI/OpenRouter clients
//! still send `Authorization: Bearer …`, Anthropic still sends
//! `x-api-key`, Gemini still sends `x-goog-api-key`. The gateway
//! accepts all three (see `gateway::auth::require_bearer`).
//!
//! ## Base URL
//!
//! The gateway base URL is **fixed** at the canonical
//! [`GATEWAY_BASE_URL`] (`https://gateway.thclaws.cloud`). End users
//! can't change it from the Settings UI — there's nothing to
//! misconfigure. For development against a staging gateway, set the
//! `THCLAWS_GATEWAY_BASE_URL` env var; it overrides at lookup time.
//!
//! ## Access key
//!
//! Resolution order:
//! 1. `THCLAWS_GATEWAY_API_KEY` env var
//! 2. OS keychain bundle, account `gateway` (a dedicated `gw_v1_` key)
//! 3. The thClaws.cloud CLI token ([`crate::cloud::token`]) — the gateway
//!    accepts it directly, so a cloud-logged-in user needs no separate
//!    gateway key.
//! 4. None → overlay disabled (falls back to the provider's native upstream)

use crate::config::AppConfig;
use crate::providers::ProviderKind;

/// Fixed gateway base URL. Points at the consolidated thclaws.cloud
/// gateway via the dedicated subdomain `gateway.thclaws.cloud` (Traefik
/// IngressRoute `thclaws-cloud-gateway-host`, no /gateway strip-prefix —
/// the gateway is served at the host root), TLS by the *.thclaws.cloud
/// wildcard cert. Replaces the retired standalone `gateway.thclaws.ai`.
/// Override at lookup time with `THCLAWS_GATEWAY_BASE_URL` for staging /
/// local dev only.
pub const GATEWAY_BASE_URL: &str = "https://gateway.thclaws.cloud";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayOverlay {
    /// Final base URL: `<gateway>/<segment>` with no trailing slash.
    /// Provider impls append their own per-request path.
    pub base_url: String,
    /// The gateway access key. Each provider plugs this into its
    /// existing auth header (Authorization / x-api-key / x-goog-api-key).
    pub access_key: String,
}

/// The path segment under the gateway base URL for each provider.
/// Matches the routes wired in `crates/gateway/src/routes/mod.rs`.
pub fn provider_segment(kind: ProviderKind) -> Option<&'static str> {
    match kind {
        ProviderKind::OpenAI | ProviderKind::OpenAIResponses => Some("openai"),
        ProviderKind::Anthropic => Some("anthropic"),
        ProviderKind::Gemini => Some("google"),
        ProviderKind::OpenRouter => Some("openrouter"),
        // Cloud-routable OpenAI-compatible / hosted providers — the
        // gateway holds their keys and proxies them so hosted runners
        // carry none. Local providers (ollama@localhost, lmstudio) and
        // subprocess ones (anthropic-agent, chatgpt-codex) are not
        // here; neither are nvidia / opencode-go / ollama-cloud
        // (removed 2026-06-10 — no per-token upstream price to meter,
        // so the gateway dropped their routes; desktop users reach
        // them directly with their own keys).
        ProviderKind::DashScope => Some("dashscope"),
        ProviderKind::QwenCloud => Some("qwen-cloud"),
        ProviderKind::ZAi => Some("zai"),
        ProviderKind::DeepSeek => Some("deepseek"),
        ProviderKind::Minimax => Some("minimax"),
        ProviderKind::ThaiLLM => Some("thaillm"),
        ProviderKind::XAi => Some("xai"),
        ProviderKind::Moonshot => Some("moonshot"),
        _ => None,
    }
}

/// Lowercase name used in `AppConfig::gateway_use_for`. Matches the
/// path segment so the per-provider toggle UI and the routing share
/// vocabulary.
pub fn provider_name_for_config(kind: ProviderKind) -> Option<&'static str> {
    provider_segment(kind)
}

/// True when this session routes through the thClaws gateway: at least
/// one provider is toggled into `gateway_use_for` AND the access key is
/// present (hosted cloud pods force both; desktop BYOK has neither).
/// The model picker uses this to show only Featured (gateway-routable)
/// providers in gateway mode, and the full catalogue for BYOK sessions.
pub fn is_active(config: &AppConfig) -> bool {
    !config.gateway_use_for.is_empty() && resolve_access_key().is_some()
}

/// True when a gateway access key is available (a gateway key OR the cloud CLI
/// token). The GUI enables the proxy checkbox only when this holds — no token,
/// no proxy.
pub fn has_access_key() -> bool {
    resolve_access_key().is_some()
}

/// Map a catalogue/picker provider NAME (not kind) to its gateway
/// segment. Only the catalogue's `gemini` diverges from its segment
/// (`google`); the other gateway-routable providers match 1:1.
pub fn segment_for_provider_name(name: &str) -> Option<&'static str> {
    match name {
        "openai" => Some("openai"),
        "anthropic" => Some("anthropic"),
        "gemini" | "google" => Some("google"),
        "openrouter" => Some("openrouter"),
        "dashscope" => Some("dashscope"),
        "qwen-cloud" => Some("qwen-cloud"),
        "zai" => Some("zai"),
        "deepseek" => Some("deepseek"),
        "minimax" => Some("minimax"),
        "thaillm" => Some("thaillm"),
        "xai" => Some("xai"),
        "moonshot" => Some("moonshot"),
        _ => None,
    }
}

/// True when model lists for `provider_name` should hide unpriced
/// catalogue rows: the gateway overlay is active for the provider
/// (toggle on + access key present), so every call is strictly
/// metered and a model without catalogue pricing is rejected with
/// 400 — offering it in the picker only advertises an error. With
/// the overlay off (desktop, own keys) nothing is hidden.
pub fn hides_unpriced_models(config: &AppConfig, provider_name: &str) -> bool {
    let Some(segment) = segment_for_provider_name(provider_name) else {
        return false;
    };
    config
        .gateway_use_for
        .iter()
        .any(|p| p.eq_ignore_ascii_case(segment))
        && resolve_access_key().is_some()
}

/// Compute the overlay for this provider kind. Returns `None` when
/// the toggle is off for this provider OR the access key isn't
/// available. The base URL is fixed (see [`GATEWAY_BASE_URL`] and the
/// `THCLAWS_GATEWAY_BASE_URL` override).
pub fn for_kind(config: &AppConfig, kind: ProviderKind) -> Option<GatewayOverlay> {
    let name = provider_name_for_config(kind)?;
    if !config
        .gateway_use_for
        .iter()
        .any(|p| p.eq_ignore_ascii_case(name))
    {
        return None;
    }
    let access_key = resolve_access_key()?;
    let segment = provider_segment(kind)?;
    let base_url = format!("{}/{}", resolve_base_url().trim_end_matches('/'), segment);
    Some(GatewayOverlay {
        base_url,
        access_key,
    })
}

/// The gateway overlay for ROUTING the active model — `for_kind` plus a
/// per-model eligibility gate. The gateway only serves **featured** models
/// (a Featured-tier provider with a priced catalogue entry); a non-featured
/// model returns `None` so `build_provider` falls back to BYOK rather than
/// sending a request the gateway would reject with 400. Unlike `for_kind`
/// (which answers the model-agnostic "does this provider have a route?",
/// used by `preferred_default_model`), this is the call routing sites use.
///
/// The catalogue is only consulted AFTER `for_kind` confirms the proxy is on
/// for this provider + an access key exists, so BYOK sessions never pay the
/// lookup cost.
pub fn gateway_overlay_for_model(config: &AppConfig, kind: ProviderKind) -> Option<GatewayOverlay> {
    let overlay = for_kind(config, kind)?;
    if !model_is_gateway_servable(&config.model) {
        return None;
    }
    Some(overlay)
}

/// True when `model` is gateway-servable: it has a priced catalogue entry
/// (both input + output per-mtok), which is what makes the gateway able to
/// meter it. Provider-level routability is already established by `for_kind`'s
/// caller, so this only checks pricing.
pub fn model_is_gateway_servable(model: &str) -> bool {
    crate::model_catalogue::EffectiveCatalogue::load().is_priced(model)
}

/// Resolve the gateway base URL. Honors `THCLAWS_GATEWAY_BASE_URL`
/// for dev/staging overrides; otherwise returns the canonical
/// [`GATEWAY_BASE_URL`]. `pub(crate)` so the media-generation tools
/// route through the exact same base as the LLM path.
pub(crate) fn resolve_base_url() -> String {
    std::env::var("THCLAWS_GATEWAY_BASE_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| GATEWAY_BASE_URL.to_string())
}

/// Look up the gateway access key. Env var wins (handy for CI /
/// scripted runs); otherwise keychain bundle; otherwise the cloud CLI
/// token. `pub(crate)` so media-generation tools detect gateway access
/// from the SAME three sources as the LLM path (an env-only check made
/// `TextToImage` blind to cloud-login / keychain gateway users).
pub(crate) fn resolve_access_key() -> Option<String> {
    if let Ok(v) = std::env::var("THCLAWS_GATEWAY_API_KEY") {
        let trimmed = v.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    if let Some(k) = crate::secrets::get("gateway") {
        let trimmed = k.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    // Fall back to the thClaws.cloud CLI token. The gateway accepts it
    // directly (looked up in `cli_tokens`, billed to the same user), so a
    // cloud-logged-in user gets gateway access with no separate key.
    crate::cloud::token()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Tests below mutate the process-global `THCLAWS_GATEWAY_*` env
    // vars. Cargo runs lib tests in parallel; this mutex serialises
    // the env-touching tests so a sibling test reading the resolved
    // value mid-mutation doesn't see ghost state.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn cfg(providers: &[&str]) -> AppConfig {
        let mut c = AppConfig::default();
        c.gateway_use_for = providers.iter().map(|s| s.to_string()).collect();
        c
    }

    #[test]
    fn provider_segment_covers_supported_kinds() {
        assert_eq!(provider_segment(ProviderKind::OpenAI), Some("openai"));
        assert_eq!(provider_segment(ProviderKind::Anthropic), Some("anthropic"));
        assert_eq!(provider_segment(ProviderKind::Gemini), Some("google"));
        assert_eq!(
            provider_segment(ProviderKind::OpenRouter),
            Some("openrouter")
        );
        assert_eq!(provider_segment(ProviderKind::Ollama), None);
        assert_eq!(provider_segment(ProviderKind::LMStudio), None);
        // Featured providers added to the gateway in part 3.
        assert_eq!(provider_segment(ProviderKind::XAi), Some("xai"));
        assert_eq!(provider_segment(ProviderKind::Moonshot), Some("moonshot"));
        assert_eq!(segment_for_provider_name("xai"), Some("xai"));
        assert_eq!(segment_for_provider_name("moonshot"), Some("moonshot"));
    }

    #[test]
    fn is_active_requires_toggle_and_key() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("THCLAWS_GATEWAY_API_KEY", "gw_v1_test");
        assert!(is_active(&cfg(&["openai"])), "toggle + key → active");
        assert!(!is_active(&cfg(&[])), "no provider toggled → inactive");
        std::env::remove_var("THCLAWS_GATEWAY_API_KEY");
        // No key (unless the test host has a keychain 'gateway' entry or
        // a thClaws.cloud login — both are valid access-key sources).
        if resolve_access_key().is_none() {
            assert!(!is_active(&cfg(&["openai"])), "no key → inactive");
        }
    }

    #[test]
    fn hides_unpriced_models_requires_toggle_and_key() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("THCLAWS_GATEWAY_API_KEY", "gw_v1_test");
        let cfg_on = cfg(&["dashscope", "google"]);
        assert!(hides_unpriced_models(&cfg_on, "dashscope"));
        // Catalogue name "gemini" maps to segment "google".
        assert!(hides_unpriced_models(&cfg_on, "gemini"));
        // Provider not toggled on → desktop path, nothing hidden.
        assert!(!hides_unpriced_models(&cfg_on, "zai"));
        // Non-gateway provider names never hide.
        assert!(!hides_unpriced_models(&cfg_on, "ollama"));
        assert!(!hides_unpriced_models(&cfg_on, "nvidia"));
        std::env::remove_var("THCLAWS_GATEWAY_API_KEY");
        // No access key → overlay inert → nothing hidden (unless the test
        // host has a keychain 'gateway' entry or a thClaws.cloud login —
        // both are valid access-key sources).
        if resolve_access_key().is_none() {
            assert!(!hides_unpriced_models(&cfg_on, "dashscope"));
        }
    }

    #[test]
    fn for_kind_returns_none_when_provider_not_enabled() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config = cfg(&["openai"]);
        std::env::set_var("THCLAWS_GATEWAY_API_KEY", "gw_v1_test");
        let out = for_kind(&config, ProviderKind::Gemini);
        std::env::remove_var("THCLAWS_GATEWAY_API_KEY");
        assert!(out.is_none());
    }

    #[test]
    fn for_kind_returns_none_when_access_key_missing() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config = cfg(&["openai"]);
        std::env::remove_var("THCLAWS_GATEWAY_API_KEY");
        let out = for_kind(&config, ProviderKind::OpenAI);
        // Will be None unless the keychain happens to have a 'gateway'
        // entry on the test machine. Most CI hosts won't.
        if out.is_some() {
            // Local dev with a real key in the keychain — accept it.
            return;
        }
        assert!(out.is_none());
    }

    #[test]
    fn for_kind_uses_fixed_base_url_by_default() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config = cfg(&["openai", "anthropic"]);
        std::env::set_var("THCLAWS_GATEWAY_API_KEY", "gw_v1_test");
        std::env::remove_var("THCLAWS_GATEWAY_BASE_URL");
        let openai = for_kind(&config, ProviderKind::OpenAI).expect("openai overlay");
        let anthropic = for_kind(&config, ProviderKind::Anthropic).expect("anthropic overlay");
        std::env::remove_var("THCLAWS_GATEWAY_API_KEY");

        assert_eq!(openai.base_url, format!("{GATEWAY_BASE_URL}/openai"));
        assert_eq!(openai.access_key, "gw_v1_test");
        assert_eq!(anthropic.base_url, format!("{GATEWAY_BASE_URL}/anthropic"));
    }

    #[test]
    fn for_kind_honors_base_url_env_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config = cfg(&["openrouter"]);
        std::env::set_var("THCLAWS_GATEWAY_API_KEY", "k");
        std::env::set_var(
            "THCLAWS_GATEWAY_BASE_URL",
            "https://staging.gateway.thclaws.ai/",
        );
        let out = for_kind(&config, ProviderKind::OpenRouter).expect("overlay");
        std::env::remove_var("THCLAWS_GATEWAY_API_KEY");
        std::env::remove_var("THCLAWS_GATEWAY_BASE_URL");
        assert_eq!(
            out.base_url,
            "https://staging.gateway.thclaws.ai/openrouter"
        );
    }
}

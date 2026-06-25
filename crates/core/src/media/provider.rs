//! Provider abstraction for media generation (dev-plan/40, Tier 1).
//!
//! Tier 1 covers images only: text→image and image→image. The
//! `ImageProvider` trait is the seam each backend (Gemini, OpenAI, …)
//! implements; the tools in `tools/image_gen.rs` and (later) the Media
//! Studio shell both resolve a model to a provider via
//! `media::registry` and call through this trait. Video (`VideoProvider`
//! + the async job model) is Tier 2 and intentionally absent here.

use crate::error::{Error, Result};
use async_trait::async_trait;

/// One input image for edit-mode (image→image). `bytes` are the raw
/// file contents; `mime` is the detected MIME (`image/png`, …).
#[derive(Debug, Clone)]
pub struct InputImage {
    pub bytes: Vec<u8>,
    pub mime: String,
}

/// A single image-generation request, provider-agnostic. `input_images`
/// empty ⇒ text→image; one or more ⇒ image→image (edit). `model` is the
/// provider-native id already resolved by the registry (e.g.
/// `gemini-3.1-flash-image`, `gpt-image-2`). `aspect_ratio` and `size`
/// are the engine's portable tiers — each provider maps them onto its
/// own knobs.
#[derive(Debug, Clone)]
pub struct ImageRequest {
    pub model: String,
    pub prompt: String,
    pub input_images: Vec<InputImage>,
    /// One of `1:1` `3:4` `4:3` `9:16` `16:9`.
    pub aspect_ratio: String,
    /// One of `512` `1K` `2K`.
    pub size: String,
}

/// Result of a successful generation — the raw bytes of the first image.
/// (Multi-image batches are a later concern; Tier 1 returns one.)
#[derive(Debug, Clone)]
pub struct ImageResult {
    pub bytes: Vec<u8>,
}

/// Static descriptor for a model a provider exposes — used by the
/// registry to resolve names and (Tier 3) to populate the Studio picker.
#[derive(Debug, Clone, Copy)]
pub struct ImageModelInfo {
    /// Provider-native id, e.g. `gemini-3.1-flash-image`.
    pub id: &'static str,
    /// Short labels the user can type instead of the full id
    /// (e.g. `flash`, `pro`). The empty string `""` marks the
    /// provider's default model (matched when no model is given).
    pub aliases: &'static [&'static str],
    /// Human label for pickers.
    pub label: &'static str,
}

#[async_trait]
pub trait ImageProvider: Send + Sync {
    /// Stable provider id (`gemini`, `openai`). Matches the value the
    /// `provider` tool param accepts and the Studio shows.
    fn id(&self) -> &'static str;

    /// Models this provider exposes.
    fn models(&self) -> &'static [ImageModelInfo];

    /// If this provider handles `raw` (a model id, an alias, or — only
    /// for the default provider — the empty string), return the
    /// resolved provider-native model id. Otherwise `None` so the
    /// registry can try the next provider.
    fn resolve_model(&self, raw: &str) -> Option<String> {
        let raw = raw.trim();
        for m in self.models() {
            if raw == m.id || m.aliases.contains(&raw) {
                return Some(m.id.to_string());
            }
        }
        None
    }

    /// Generate (or edit) an image. Validates its own credentials and
    /// returns a clear `Error::Tool` if they're missing — the multi-
    /// provider tools no longer hide on a single provider's key.
    async fn generate(&self, req: &ImageRequest) -> Result<ImageResult>;
}

// ── Video (Tier 2, dev-plan/40) ──────────────────────────────────
//
// Video generation is asynchronous: providers submit a long-running
// job and the caller polls. So `VideoProvider` splits into `submit`
// (returns an opaque provider job ref) and `poll` (returns the current
// state, downloading bytes when done). The `media::job` store persists
// the ref so polling survives a restart; the `TextToVideo` /
// `ImageToVideo` tools submit, and `MediaJobStatus` polls.

/// A video-generation request, provider-agnostic. `init_image` empty ⇒
/// text→video; present ⇒ image→video (the image conditions the first
/// frame). `aspect_ratio` is the engine's portable tier; each provider
/// maps it onto what it accepts (Veo only takes 9:16 / 16:9).
#[derive(Debug, Clone)]
pub struct VideoRequest {
    pub model: String,
    pub prompt: String,
    pub init_image: Option<InputImage>,
    pub aspect_ratio: String,
    pub duration_seconds: u32,
    /// Output resolution tier, e.g. `720P` / `1080P`. Veo ignores it
    /// (resolution follows its aspect); DashScope video (happyhorse)
    /// takes it as a `resolution` parameter that changes pricing.
    pub resolution: String,
}

/// Opaque handle to a provider-side job (e.g. a Veo long-running
/// operation name). Stored in the job log so a poll can resume after a
/// restart.
#[derive(Debug, Clone)]
pub struct ProviderJobRef {
    pub op: String,
}

/// State of a video job as reported by `poll`. `Done` carries the
/// downloaded bytes (the provider fetches them, since only it knows the
/// gateway URL-rewrite); the tool layer writes them to the workspace.
#[derive(Debug, Clone)]
pub enum JobState {
    Running { pct: Option<u8> },
    Done { bytes: Vec<u8> },
    Failed { msg: String },
}

#[async_trait]
pub trait VideoProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn models(&self) -> &'static [ImageModelInfo];
    fn resolve_model(&self, raw: &str) -> Option<String> {
        let raw = raw.trim();
        for m in self.models() {
            if raw == m.id || m.aliases.contains(&raw) {
                return Some(m.id.to_string());
            }
        }
        None
    }

    /// Submit a video job; returns the provider job ref to poll. Fast
    /// (a single POST) — does NOT wait for the render.
    async fn submit(&self, req: &VideoRequest) -> Result<ProviderJobRef>;

    /// Poll a previously-submitted job once. Returns `Running` while in
    /// flight, `Done { bytes }` (downloaded) on success, `Failed` on a
    /// terminal error envelope.
    async fn poll(&self, job: &ProviderJobRef) -> Result<JobState>;
}

/// Resolved HTTP target for a provider call: where to POST and which
/// key to present. The provider applies its own auth-header *scheme*
/// (Gemini → `x-goog-api-key`, OpenAI → `Authorization: Bearer`).
#[derive(Debug, Clone)]
pub struct ResolvedEndpoint {
    pub base_url: String,
    pub api_key: String,
}

/// Resolve `(base_url, api_key)` for a provider, mirroring the
/// native-or-gateway logic the old `gemini_image::resolve_endpoint`
/// hard-coded — now shared so every provider bills the same way:
///
/// 1. A real native key (one of `native_key_vars`, non-empty, not the
///    `gateway-placeholder` sentinel) → call the upstream directly.
/// 2. Otherwise, if a thClaws Gateway key is present, route through
///    `<gateway>/<segment>` so the gateway injects the upstream key and
///    meters the call (per dev-plan/40 + project_gateway_overlay).
/// 3. Otherwise, fall back to whatever native key we have, or error.
pub fn resolve_endpoint(
    native_key_vars: &[&str],
    native_base: &str,
    gateway_segment: &str,
) -> Result<ResolvedEndpoint> {
    let native = resolve_native_key(native_key_vars);
    if let Some(ref key) = native {
        if key != "gateway-placeholder" {
            return Ok(ResolvedEndpoint {
                base_url: native_base.to_string(),
                api_key: key.clone(),
            });
        }
    }
    if let Some(gw_key) = gateway_key() {
        let base = gateway_base();
        return Ok(ResolvedEndpoint {
            base_url: format!("{}/{}", base.trim_end_matches('/'), gateway_segment),
            api_key: gw_key,
        });
    }
    native
        .map(|k| ResolvedEndpoint {
            base_url: native_base.to_string(),
            api_key: k,
        })
        .ok_or_else(|| {
            Error::Tool(format!(
                "no API key for image generation — set one of {native_key_vars:?}, or enable the thClaws Gateway (sign in to thClaws.cloud, add a `gateway` key, or set THCLAWS_GATEWAY_API_KEY)"
            ))
        })
}

/// First non-empty env var among `vars`, with wrapping-quote strip —
/// the same defensive sanitisation provider keys get.
fn resolve_native_key(vars: &[&str]) -> Option<String> {
    for var in vars {
        if let Ok(raw) = std::env::var(var) {
            let trimmed = raw.trim();
            let bytes = trimmed.as_bytes();
            let cleaned = if bytes.len() >= 2
                && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
                    || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
            {
                &trimmed[1..trimmed.len() - 1]
            } else {
                trimmed
            };
            if !cleaned.is_empty() {
                return Some(cleaned.to_string());
            }
        }
    }
    None
}

/// Gateway access key, resolved from the SAME three sources the LLM
/// gateway uses (`THCLAWS_GATEWAY_API_KEY` env → `gateway` keychain
/// bundle → thClaws.cloud CLI token). Previously this read only the env
/// var, so cloud-login / keychain gateway users — whose chat works fine —
/// hit "no API key" on TextToImage even though they had gateway access.
fn gateway_key() -> Option<String> {
    crate::providers::thclaws_gateway::resolve_access_key()
}

fn gateway_base() -> String {
    crate::providers::thclaws_gateway::resolve_base_url()
}

//! OpenAI image provider (dev-plan/40, Tier 1).
//!
//! `gpt-image-2` — OpenAI's current image model. text→image hits
//! `POST /v1/images/generations` (JSON); image→image hits
//! `POST /v1/images/edits` (multipart form). Both return base64 PNG in
//! `data[0].b64_json` for the gpt-image family (no URL fetch needed).
//!
//! Native calls use `OPENAI_API_KEY` against `api.openai.com`; with only
//! the thClaws Gateway key present, calls route through `<gateway>/openai`
//! (passthrough wire shape) so the gateway meters them. Auth header is
//! `Authorization: Bearer <key>` in both cases.
//!
//! NOTE (dev-plan/40 follow-up): gateway billing of `gpt-image-2`
//! requires a verified pricing row in the model catalogue + gateway
//! allowlist (à la migration 018 for the gemini image models). Until
//! that lands, gateway-routed gpt-image-2 calls will 400 (unpriced);
//! desktop users with their own `OPENAI_API_KEY` work today.

use crate::error::{Error, Result};
use crate::media::provider::{ImageModelInfo, ImageProvider, ImageRequest, ImageResult};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

const OPENAI_BASE: &str = "https://api.openai.com";

const MODELS: &[ImageModelInfo] = &[ImageModelInfo {
    id: "gpt-image-2",
    aliases: &["openai", "gpt-image", "gpt-image-2"],
    label: "OpenAI GPT Image 2",
}];

pub struct OpenAiImageProvider;

impl OpenAiImageProvider {
    /// Map the engine's portable aspect tiers onto gpt-image's fixed
    /// size set. Square → 1024², portrait → 1024×1536, landscape →
    /// 1536×1024.
    fn size(req: &ImageRequest) -> &'static str {
        match req.aspect_ratio.as_str() {
            "1:1" => "1024x1024",
            "3:4" | "9:16" => "1024x1536",
            _ => "1536x1024",
        }
    }
    /// Map the size tier onto gpt-image's quality knob.
    fn quality(req: &ImageRequest) -> &'static str {
        match req.size.as_str() {
            "512" => "low",
            "2K" => "high",
            _ => "medium",
        }
    }

    fn decode_first(v: &Value) -> Result<Vec<u8>> {
        let b64 = v
            .pointer("/data/0/b64_json")
            .and_then(|x| x.as_str())
            .ok_or_else(|| Error::Tool("openai response missing /data/0/b64_json".into()))?;
        B64.decode(b64)
            .map_err(|e| Error::Tool(format!("base64 decode: {e}")))
    }

    fn client() -> Result<Client> {
        Client::builder()
            .timeout(Duration::from_secs(180))
            .build()
            .map_err(|e| Error::Tool(format!("http client: {e}")))
    }
}

#[async_trait]
impl ImageProvider for OpenAiImageProvider {
    fn id(&self) -> &'static str {
        "openai"
    }
    fn models(&self) -> &'static [ImageModelInfo] {
        MODELS
    }

    async fn generate(&self, req: &ImageRequest) -> Result<ImageResult> {
        let ep =
            crate::media::provider::resolve_endpoint(&["OPENAI_API_KEY"], OPENAI_BASE, "openai")?;
        let size = Self::size(req);
        let quality = Self::quality(req);
        let client = Self::client()?;

        if req.input_images.is_empty() {
            // text→image: JSON generations endpoint.
            let body = json!({
                "model": req.model,
                "prompt": req.prompt,
                "size": size,
                "quality": quality,
                "n": 1,
                "output_format": "png",
            });
            let url = format!(
                "{}/v1/images/generations",
                ep.base_url.trim_end_matches('/')
            );
            let resp = crate::multi_tenant::attach_member(client.post(&url))
                .bearer_auth(&ep.api_key)
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await
                .map_err(|e| Error::Tool(format!("http: {e}")))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let b = resp.text().await.unwrap_or_default();
                return Err(Error::Tool(format!(
                    "openai http {status}: {}",
                    &b[..b.len().min(400)]
                )));
            }
            let v: Value = resp
                .json()
                .await
                .map_err(|e| Error::Tool(format!("openai response not json: {e}")))?;
            return Ok(ImageResult {
                bytes: Self::decode_first(&v)?,
            });
        }

        // image→image: multipart edits endpoint. gpt-image-2 accepts
        // one or more `image` parts; Tier 1 sends the first.
        let img = &req.input_images[0];
        let ext = match img.mime.as_str() {
            "image/jpeg" => "jpg",
            "image/webp" => "webp",
            _ => "png",
        };
        let part = reqwest::multipart::Part::bytes(img.bytes.clone())
            .file_name(format!("input.{ext}"))
            .mime_str(&img.mime)
            .map_err(|e| Error::Tool(format!("multipart part: {e}")))?;
        let form = reqwest::multipart::Form::new()
            .text("model", req.model.clone())
            .text("prompt", req.prompt.clone())
            .text("size", size)
            .text("quality", quality)
            .text("n", "1")
            .part("image", part);
        let url = format!("{}/v1/images/edits", ep.base_url.trim_end_matches('/'));
        let resp = crate::multi_tenant::attach_member(client.post(&url))
            .bearer_auth(&ep.api_key)
            .multipart(form)
            .send()
            .await
            .map_err(|e| Error::Tool(format!("http: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let b = resp.text().await.unwrap_or_default();
            return Err(Error::Tool(format!(
                "openai http {status}: {}",
                &b[..b.len().min(400)]
            )));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::Tool(format!("openai response not json: {e}")))?;
        Ok(ImageResult {
            bytes: Self::decode_first(&v)?,
        })
    }
}

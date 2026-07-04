//! DashScope video provider (dev-plan/40) — Alibaba Model Studio
//! async video synthesis.
//!
//! happyhorse-1.0-t2v is a text→video model on DashScope's async
//! `video-generation/video-synthesis` endpoint: submit with
//! `X-DashScope-Async: enable` → `output.task_id`, then poll
//! `/api/v1/tasks/<id>` until `task_status: SUCCEEDED` and download
//! `output.video_url`. International endpoint `dashscope-intl.aliyuncs.com`
//! (verified to host the model); auth `Authorization: Bearer
//! DASHSCOPE_API_KEY`.
//!
//! Pricing is PER SECOND of output ($0.14/s at 720P, $0.24/s at 1080P;
//! recorded as `price_per_video_second_usd` in the catalogue). Desktop
//! users with a native DASHSCOPE_API_KEY work today; gateway per-second
//! metering is a follow-up.

use crate::error::{Error, Result};
use crate::media::provider::{
    ImageModelInfo, JobState, ProviderJobRef, VideoProvider, VideoRequest,
};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

const DASHSCOPE_BASE: &str = "https://dashscope-intl.aliyuncs.com";
const SUBMIT_PATH: &str = "/api/v1/services/aigc/video-generation/video-synthesis";

const MODELS: &[ImageModelInfo] = &[
    ImageModelInfo {
        id: "happyhorse-1.0-t2v",
        aliases: &["happyhorse", "happyhorse-1.0-t2v"],
        label: "HappyHorse 1.0 (text→video)",
    },
    ImageModelInfo {
        id: "happyhorse-1.0-i2v",
        aliases: &["happyhorse-i2v", "happyhorse-1.0-i2v"],
        label: "HappyHorse 1.0 (image→video)",
    },
];

pub struct DashScopeVideoProvider;

impl DashScopeVideoProvider {
    fn resolution(req: &VideoRequest) -> &str {
        match req.resolution.as_str() {
            "1080P" | "1080p" => "1080P",
            _ => "720P",
        }
    }
    fn client(timeout_secs: u64) -> Result<Client> {
        Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .map_err(|e| Error::Tool(format!("http client: {e}")))
    }
}

#[async_trait]
impl VideoProvider for DashScopeVideoProvider {
    fn id(&self) -> &'static str {
        "dashscope"
    }
    fn models(&self) -> &'static [ImageModelInfo] {
        MODELS
    }
    fn resolve_model(&self, raw: &str) -> Option<String> {
        let raw = raw.trim();
        for m in MODELS {
            if raw == m.id || m.aliases.contains(&raw) {
                return Some(m.id.to_string());
            }
        }
        // Forward-compat: accept any future `happyhorse-*` id.
        if raw.starts_with("happyhorse") {
            return Some(raw.to_string());
        }
        None
    }

    async fn submit(&self, req: &VideoRequest) -> Result<ProviderJobRef> {
        let ep = crate::media::provider::resolve_endpoint(
            &["DASHSCOPE_API_KEY"],
            DASHSCOPE_BASE,
            "dashscope",
        )?;
        // text→video (happyhorse-1.0-t2v) is prompt-only + a `ratio`
        // parameter; image→video (happyhorse-1.0-i2v) carries the source
        // frame in `input.media[].first_frame` (a base64 data URI for a
        // local image — verified accepted) and derives aspect from the
        // frame, so `ratio` is omitted.
        let mut input = json!({ "prompt": req.prompt });
        let mut parameters = json!({
            "resolution": Self::resolution(req),
            "duration": req.duration_seconds,
        });
        if let Some(img) = &req.init_image {
            input["media"] = json!([{
                "type": "first_frame",
                "url": format!("data:{};base64,{}", img.mime, B64.encode(&img.bytes)),
            }]);
        } else {
            parameters["ratio"] = json!(req.aspect_ratio);
        }
        let body = json!({
            "model": req.model,
            "input": input,
            "parameters": parameters,
        });
        let url = format!("{}{}", ep.base_url.trim_end_matches('/'), SUBMIT_PATH);
        let client = Self::client(60)?;
        let resp = crate::multi_tenant::attach_member(client.post(&url))
            .bearer_auth(&ep.api_key)
            .header("X-DashScope-Async", "enable")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Tool(format!("dashscope video submit http: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let b = resp.text().await.unwrap_or_default();
            return Err(Error::Tool(format!(
                "dashscope video submit http {status}: {}",
                b.chars().take(400).collect::<String>()
            )));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::Tool(format!("dashscope video submit not json: {e}")))?;
        let task_id = v
            .pointer("/output/task_id")
            .and_then(|t| t.as_str())
            .ok_or_else(|| Error::Tool("dashscope video submit missing output.task_id".into()))?;
        Ok(ProviderJobRef {
            op: task_id.to_string(),
        })
    }

    async fn poll(&self, job: &ProviderJobRef) -> Result<JobState> {
        let ep = crate::media::provider::resolve_endpoint(
            &["DASHSCOPE_API_KEY"],
            DASHSCOPE_BASE,
            "dashscope",
        )?;
        let url = format!(
            "{}/api/v1/tasks/{}",
            ep.base_url.trim_end_matches('/'),
            job.op
        );
        let client = Self::client(30)?;
        let resp = client
            .get(&url)
            .bearer_auth(&ep.api_key)
            .send()
            .await
            .map_err(|e| Error::Tool(format!("dashscope video poll http: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let b = resp.text().await.unwrap_or_default();
            return Err(Error::Tool(format!(
                "dashscope video poll http {status}: {}",
                b.chars().take(400).collect::<String>()
            )));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::Tool(format!("dashscope video poll not json: {e}")))?;
        let status = v
            .pointer("/output/task_status")
            .and_then(|s| s.as_str())
            .unwrap_or("UNKNOWN");
        match status {
            "PENDING" | "RUNNING" => Ok(JobState::Running { pct: None }),
            "SUCCEEDED" => {
                let video_url = v
                    .pointer("/output/video_url")
                    .and_then(|u| u.as_str())
                    .or_else(|| v.pointer("/output/results/0/url").and_then(|u| u.as_str()))
                    .ok_or_else(|| {
                        Error::Tool("dashscope video done but no output.video_url".into())
                    })?;
                let dl = Self::client(180)?;
                let bytes = dl
                    .get(video_url)
                    .send()
                    .await
                    .map_err(|e| Error::Tool(format!("video download: {e}")))?
                    .bytes()
                    .await
                    .map(|b| b.to_vec())
                    .map_err(|e| Error::Tool(format!("video body: {e}")))?;
                Ok(JobState::Done { bytes })
            }
            other => {
                let msg = v
                    .pointer("/output/message")
                    .and_then(|m| m.as_str())
                    .unwrap_or(other);
                Ok(JobState::Failed {
                    msg: format!("dashscope video {other}: {msg}"),
                })
            }
        }
    }
}

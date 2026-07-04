//! Kie.ai jobs client (createTask / recordInfo). T0 facts baked in:
//! Cloudflare 403s default UAs (error 1010) — browser-normal UA on
//! every call; results classify on `data.state` (the top-level `code`
//! is unreliable — a successful poll once arrived as `code:505`);
//! credits debit at submit; insufficient balance is `code:500` with a
//! prose message, not a structured error.

use super::USER_AGENT;
use crate::error::{Error, Result};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const JOBS_BASE: &str = "https://api.kie.ai";
const POLL_INTERVAL: Duration = Duration::from_secs(15);
/// Generous ceiling — T0 shots rendered in 2–4 min; 4K may take longer.
const POLL_TIMEOUT: Duration = Duration::from_secs(20 * 60);

pub(crate) fn jobs_base() -> String {
    std::env::var("KIE_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| JOBS_BASE.to_string())
}

#[derive(Debug, Clone)]
pub struct TaskResult {
    pub clip_url: String,
    pub credits: Option<f64>,
}

pub struct KieClient {
    api_key: String,
    base: String,
    client: reqwest::Client,
}

impl KieClient {
    pub fn new(api_key: String) -> Self {
        Self::with_base(api_key, jobs_base())
    }

    pub fn with_base(api_key: String, base: String) -> Self {
        Self {
            api_key,
            base,
            client: reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .build()
                .expect("reqwest client"),
        }
    }

    /// BYOK-or-gateway endpoint (dev-plan/53 Stage D): a real
    /// `KIE_API_KEY` calls api.kie.ai directly; otherwise a gateway key
    /// routes through `<gateway>/kie` where the platform key is
    /// injected and the job is metered (billed from the poll's
    /// creditsConsumed).
    pub fn resolve() -> Result<Self> {
        let ep = crate::media::provider::resolve_endpoint(&["KIE_API_KEY"], &jobs_base(), "kie")?;
        Ok(Self::with_base(ep.api_key, ep.base_url))
    }

    pub async fn create_task(&self, payload: &Value) -> Result<String> {
        let resp: Value = crate::multi_tenant::attach_member(
            self.client
                .post(format!("{}/api/v1/jobs/createTask", self.base)),
        )
        .bearer_auth(&self.api_key)
        .json(payload)
        .send()
        .await
        .map_err(|e| Error::Tool(format!("kie createTask: {e}")))?
        .json()
        .await
        .map_err(|e| Error::Tool(format!("kie createTask response: {e}")))?;

        if let Some(task_id) = resp["data"]["taskId"].as_str() {
            return Ok(task_id.to_string());
        }
        let msg = resp["msg"].as_str().unwrap_or("");
        if msg.to_lowercase().contains("credits insufficient") {
            return Err(Error::Tool(
                "Kie balance is empty — top up at kie.ai, then resume the job".into(),
            ));
        }
        Err(Error::Tool(format!(
            "kie createTask failed: {}",
            resp.to_string().chars().take(250).collect::<String>()
        )))
    }

    /// Poll until terminal state or timeout. `cancel` is checked every
    /// interval so FilmJobCancel stops the wait promptly (the Kie task
    /// itself keeps running server-side — its cost is already spent).
    pub async fn poll(&self, task_id: &str, cancel: &AtomicBool) -> Result<TaskResult> {
        let started = std::time::Instant::now();
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Tool("job cancelled".into()));
            }
            if started.elapsed() > POLL_TIMEOUT {
                return Err(Error::Tool(format!(
                    "kie task {task_id} still not terminal after {}s",
                    POLL_TIMEOUT.as_secs()
                )));
            }

            let resp: Value = crate::multi_tenant::attach_member(self.client.get(format!(
                "{}/api/v1/jobs/recordInfo?taskId={task_id}",
                self.base
            )))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| Error::Tool(format!("kie recordInfo: {e}")))?
            .json()
            .await
            .map_err(|e| Error::Tool(format!("kie recordInfo response: {e}")))?;

            let data = &resp["data"];
            match data["state"].as_str() {
                Some("success") => {
                    let result: Value = data["resultJson"]
                        .as_str()
                        .and_then(|s| serde_json::from_str(s).ok())
                        .unwrap_or(Value::Null);
                    let clip_url = result["resultUrls"][0]
                        .as_str()
                        .ok_or_else(|| {
                            Error::Tool(format!("kie task {task_id}: success but no resultUrls"))
                        })?
                        .to_string();
                    return Ok(TaskResult {
                        clip_url,
                        credits: data["creditsConsumed"].as_f64(),
                    });
                }
                Some("fail") => {
                    return Err(Error::Tool(format!(
                        "kie task {task_id} failed: {} {}",
                        data["failCode"].as_str().unwrap_or(""),
                        data["failMsg"].as_str().unwrap_or("(no message)")
                    )));
                }
                // "waiting" covers queued AND generating (T0 observation).
                _ => tokio::time::sleep(POLL_INTERVAL).await,
            }
        }
    }

    /// Kie's Veo endpoint (`/api/v1/veo/generate` + `/veo/record-info`) —
    /// same host + key as the jobs API, different route + response
    /// (`successFlag` 1=ok / 2,3=fail; the clip is the first mp4 in the
    /// payload). Used for the `veo` backend (REFERENCE_2_VIDEO).
    pub async fn create_veo_task(&self, payload: &Value) -> Result<String> {
        let resp: Value = crate::multi_tenant::attach_member(
            self.client
                .post(format!("{}/api/v1/veo/generate", self.base)),
        )
        .bearer_auth(&self.api_key)
        .json(payload)
        .send()
        .await
        .map_err(|e| Error::Tool(format!("kie veo generate: {e}")))?
        .json()
        .await
        .map_err(|e| Error::Tool(format!("kie veo generate response: {e}")))?;
        resp["data"]["taskId"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| {
                Error::Tool(format!(
                    "kie veo generate failed: {}",
                    resp.to_string().chars().take(250).collect::<String>()
                ))
            })
    }

    pub async fn poll_veo(&self, task_id: &str, cancel: &AtomicBool) -> Result<TaskResult> {
        let started = std::time::Instant::now();
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Tool("job cancelled".into()));
            }
            if started.elapsed() > POLL_TIMEOUT {
                return Err(Error::Tool(format!(
                    "kie veo task {task_id} still not terminal after {}s",
                    POLL_TIMEOUT.as_secs()
                )));
            }
            let resp: Value = crate::multi_tenant::attach_member(self.client.get(format!(
                "{}/api/v1/veo/record-info?taskId={task_id}",
                self.base
            )))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| Error::Tool(format!("kie veo record-info: {e}")))?
            .json()
            .await
            .map_err(|e| Error::Tool(format!("kie veo record-info response: {e}")))?;
            match resp["data"]["successFlag"].as_i64() {
                Some(1) => {
                    let clip_url = first_mp4_url(&resp["data"]).ok_or_else(|| {
                        Error::Tool(format!("kie veo {task_id}: success but no mp4 url"))
                    })?;
                    return Ok(TaskResult {
                        clip_url,
                        credits: resp["data"]["creditsConsumed"].as_f64(),
                    });
                }
                Some(2) | Some(3) => {
                    return Err(Error::Tool(format!(
                        "kie veo task {task_id} failed: {}",
                        resp["data"]["errorMessage"]
                            .as_str()
                            .unwrap_or("(no message)")
                    )));
                }
                _ => tokio::time::sleep(POLL_INTERVAL).await,
            }
        }
    }

    pub async fn download(&self, url: &str, to: &std::path::Path) -> Result<()> {
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| Error::Tool(format!("clip download: {e}")))?
            .bytes()
            .await
            .map_err(|e| Error::Tool(format!("clip download body: {e}")))?;
        std::fs::write(to, &bytes)?;
        Ok(())
    }
}

/// First `https://…​.mp4` string anywhere in a JSON value — Kie's Veo
/// record-info nests the clip URL under varying keys across models, so a
/// scan is more robust than a fixed path.
pub(crate) fn first_mp4_url(v: &Value) -> Option<String> {
    fn walk(v: &Value, out: &mut Option<String>) {
        if out.is_some() {
            return;
        }
        match v {
            Value::String(s) if s.starts_with("http") && s.contains(".mp4") => {
                *out = Some(s.clone())
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            Value::Object(o) => o.values().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    let mut out = None;
    walk(v, &mut out);
    out
}

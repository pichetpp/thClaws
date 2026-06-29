//! HTTP client for the thClaws.cloud catalog backend.

use serde::{Deserialize, Serialize};

pub struct Client {
    base_url: String,
    http: reqwest::Client,
    token: Option<String>,
}

impl Client {
    pub fn new(base_url: impl Into<String>, token: Option<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .user_agent(concat!("thclaws-cli/", env!("CARGO_PKG_VERSION")))
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .expect("reqwest client"),
            token,
        }
    }

    fn auth_header(&self) -> Result<String, String> {
        let t = self
            .token
            .as_deref()
            .ok_or("not logged in — paste your CLI token in Settings → thClaws.cloud (mint one at /dashboard)")?;
        Ok(format!("Bearer {}", t))
    }

    pub async fn me(&self) -> Result<Me, String> {
        let auth = self.auth_header()?;
        let res = self
            .http
            .get(format!("{}/api/auth/me", self.base_url))
            .header("Authorization", auth)
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if !res.status().is_success() {
            return Err(format!(
                "status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        res.json().await.map_err(|e| format!("decode: {}", e))
    }

    pub async fn list_agents(&self, mine: bool) -> Result<Vec<AgentSummary>, String> {
        let mut url = format!("{}/api/agents", self.base_url);
        if mine {
            url.push_str("?mine=true");
        }
        let mut req = self.http.get(&url);
        if let Some(t) = &self.token {
            req = req.header("Authorization", format!("Bearer {}", t));
        }
        let res = req.send().await.map_err(|e| format!("network: {}", e))?;
        if !res.status().is_success() {
            return Err(format!(
                "status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        res.json().await.map_err(|e| format!("decode: {}", e))
    }

    pub async fn publish(&self, tarball: Vec<u8>) -> Result<PublishResult, String> {
        let auth = self.auth_header()?;
        let part = reqwest::multipart::Part::bytes(tarball)
            .file_name("agent.tar.gz")
            .mime_str("application/gzip")
            .map_err(|e| format!("mime: {}", e))?;
        let form = reqwest::multipart::Form::new().part("file", part);

        let res = self
            .http
            .post(format!("{}/api/agents/publish", self.base_url))
            .header("Authorization", auth)
            .multipart(form)
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if !res.status().is_success() {
            return Err(format!(
                "status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        res.json().await.map_err(|e| format!("decode: {}", e))
    }

    pub async fn download(
        &self,
        slug: &str,
        version: Option<&str>,
    ) -> Result<DownloadResult, String> {
        let auth = self.auth_header()?;
        let mut url = format!("{}/api/agents/{}/download", self.base_url, slug);
        if let Some(v) = version {
            url.push_str(&format!("?version={}", urlencoding::encode(v)));
        }
        let res = self
            .http
            .get(&url)
            .header("Authorization", auth)
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if !res.status().is_success() {
            return Err(format!(
                "status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        let version = res
            .headers()
            .get("x-agent-version")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let sha256 = res
            .headers()
            .get("x-agent-sha256")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let uuid = res
            .headers()
            .get("x-agent-uuid")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let bytes = res
            .bytes()
            .await
            .map_err(|e| format!("body: {}", e))?
            .to_vec();
        Ok(DownloadResult {
            version,
            sha256,
            uuid,
            bytes,
        })
    }

    // ---- workspace sync (dev-plan/51) ----

    /// Exchange the stored CLI token for a short-lived session JWT, used as
    /// Bearer to the hosted-workspace runner ingress.
    pub async fn cli_exchange(&self) -> Result<String, String> {
        let auth = self.auth_header()?;
        let res = self
            .http
            .post(format!("{}/api/auth/cli-exchange", self.base_url))
            .header("Authorization", auth)
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if !res.status().is_success() {
            return Err(format!(
                "cli-exchange status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        let r: CliExchangeResp = res.json().await.map_err(|e| format!("decode: {}", e))?;
        Ok(r.token)
    }

    /// List the caller's hosted workspaces.
    pub async fn list_workspaces(&self) -> Result<Vec<WorkspaceSummary>, String> {
        let auth = self.auth_header()?;
        let res = self
            .http
            .get(format!("{}/api/hosted/workspaces", self.base_url))
            .header("Authorization", auth)
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if !res.status().is_success() {
            return Err(format!(
                "list-workspaces status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        res.json()
            .await
            .map_err(|e| format!("decode workspaces: {}", e))
    }

    /// `GET <ws_url>/workspace/sync/stat` (runner ingress, JWT auth).
    pub async fn ws_sync_stat(&self, ws_url: &str, jwt: &str) -> Result<SyncStatResp, String> {
        let res = self
            .http
            .get(format!(
                "{}/workspace/sync/stat",
                ws_url.trim_end_matches('/')
            ))
            .header("Authorization", format!("Bearer {}", jwt))
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if !res.status().is_success() {
            return Err(format!(
                "sync/stat status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        res.json().await.map_err(|e| format!("decode stat: {}", e))
    }

    /// Download the cloud workspace as a `.tar.gz`.
    pub async fn ws_sync_pull(
        &self,
        ws_url: &str,
        jwt: &str,
        include_runtime: bool,
    ) -> Result<Vec<u8>, String> {
        let mut url = format!("{}/workspace/sync/pull", ws_url.trim_end_matches('/'));
        if include_runtime {
            url.push_str("?include_runtime=true");
        }
        let res = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {}", jwt))
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if res.status() == reqwest::StatusCode::CONFLICT {
            return Err("cloud workspace is busy (active turn) — try again when idle".into());
        }
        if !res.status().is_success() {
            return Err(format!(
                "sync/pull status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        Ok(res
            .bytes()
            .await
            .map_err(|e| format!("body: {}", e))?
            .to_vec())
    }

    /// Upload a `.tar.gz` to the cloud workspace. `workspace_id` records the
    /// binding on the runner.
    pub async fn ws_sync_push(
        &self,
        ws_url: &str,
        jwt: &str,
        tarball: Vec<u8>,
        delete: bool,
        workspace_id: &str,
    ) -> Result<SyncPushResp, String> {
        let url = format!(
            "{}/workspace/sync/push?delete={}&workspace_id={}",
            ws_url.trim_end_matches('/'),
            delete,
            urlencoding::encode(workspace_id)
        );
        let res = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", jwt))
            .header("Content-Type", "application/gzip")
            .body(tarball)
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if res.status() == reqwest::StatusCode::CONFLICT {
            return Err("cloud workspace is busy (active turn) — try again when idle".into());
        }
        if !res.status().is_success() {
            return Err(format!(
                "sync/push status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        res.json().await.map_err(|e| format!("decode push: {}", e))
    }

    // ---- P2 incremental ----

    /// GET the runner's content manifest. `Ok(None)` when the runner predates
    /// P2 (404) — the caller falls back to the full-tarball path.
    pub async fn ws_sync_manifest(
        &self,
        ws_url: &str,
        jwt: &str,
    ) -> Result<Option<Vec<crate::cloud::wssync::FileEntry>>, String> {
        let res = self
            .http
            .get(format!(
                "{}/workspace/sync/manifest",
                ws_url.trim_end_matches('/')
            ))
            .header("Authorization", format!("Bearer {}", jwt))
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if res.status() == reqwest::StatusCode::CONFLICT {
            return Err("cloud workspace is busy (active turn) — try again when idle".into());
        }
        if !res.status().is_success() {
            return Err(format!(
                "sync/manifest status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        Ok(Some(
            res.json()
                .await
                .map_err(|e| format!("decode manifest: {}", e))?,
        ))
    }

    /// POST a path list; download a partial `.tar.gz` of just those files.
    pub async fn ws_sync_export(
        &self,
        ws_url: &str,
        jwt: &str,
        paths: &[String],
    ) -> Result<Vec<u8>, String> {
        let res = self
            .http
            .post(format!(
                "{}/workspace/sync/export",
                ws_url.trim_end_matches('/')
            ))
            .header("Authorization", format!("Bearer {}", jwt))
            .json(paths)
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if !res.status().is_success() {
            return Err(format!(
                "sync/export status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        Ok(res
            .bytes()
            .await
            .map_err(|e| format!("body: {}", e))?
            .to_vec())
    }

    /// POST a path list to move to `.sync-trash/` on the runner.
    pub async fn ws_sync_trash(
        &self,
        ws_url: &str,
        jwt: &str,
        paths: &[String],
    ) -> Result<SyncPushResp, String> {
        let res = self
            .http
            .post(format!(
                "{}/workspace/sync/trash",
                ws_url.trim_end_matches('/')
            ))
            .header("Authorization", format!("Bearer {}", jwt))
            .json(paths)
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if !res.status().is_success() {
            return Err(format!(
                "sync/trash status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        res.json().await.map_err(|e| format!("decode trash: {}", e))
    }

    /// Wake (resume) a paused workspace pod (dev-plan/51 P3a auto-resume).
    pub async fn wake_workspace(&self, id: &str) -> Result<(), String> {
        let auth = self.auth_header()?;
        let res = self
            .http
            .post(format!(
                "{}/api/hosted/workspaces/{}/wake",
                self.base_url, id
            ))
            .header("Authorization", auth)
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if !res.status().is_success() {
            return Err(format!(
                "wake status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CliExchangeResp {
    pub token: String,
    #[serde(default)]
    pub expires_in: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceSummary {
    pub id: String,
    pub slug: String,
    pub url: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub busy: bool,
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SyncStatResp {
    #[serde(default)]
    pub file_count: usize,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub empty: bool,
    #[serde(default)]
    pub busy: bool,
    #[serde(default)]
    pub workspace_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SyncPushResp {
    #[serde(default)]
    pub written: usize,
    #[serde(default)]
    pub deleted: usize,
    #[serde(default)]
    pub trashed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Me {
    pub email: String,
    pub display_name: Option<String>,
    pub can_publish: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSummary {
    pub slug: String,
    pub name: String,
    pub description: String,
    pub categories: Vec<String>,
    pub tags: Vec<String>,
    pub current_version: Option<String>,
    pub purchase_usd: f64,
    pub author_handle: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishResult {
    pub slug: String,
    pub version: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub url: String,
    /// Server-assigned UUID for this agent. Stable across versions and
    /// across folder renames — the CLI writes this back to
    /// `./.thclaws/settings.json::agent.uuid` so re-publish from the
    /// same folder targets the same catalog entry.
    pub uuid: String,
}

#[derive(Debug)]
pub struct DownloadResult {
    pub version: String,
    pub sha256: String,
    /// Server-authoritative UUID from the `X-Agent-UUID` header.
    /// Preferred over peeking inside the tarball — the on-disk
    /// manifest.json may pre-date the Option-A identity split.
    pub uuid: Option<String>,
    pub bytes: Vec<u8>,
}

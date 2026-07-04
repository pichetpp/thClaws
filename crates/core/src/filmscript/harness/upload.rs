//! File → public URL via Kie's File Upload API (v1's only uploader —
//! zero-setup for BYOK, and hosted keyless users ride the T6 gateway
//! pass-through later; the S3/MinIO impl is deferred, dev-plan/52).
//!
//! Kie uploads are free but auto-delete after ~3 days, so the cache
//! stores `{content hash → url, expires_at}` and transparently
//! re-uploads on expiry — a re-render a week later must not 404.
//! T0 facts baked in: the response carries `downloadUrl` (the
//! quickstart's `fileUrl` is absent from stream uploads), and requests
//! need the browser-normal UA.

use super::{atomic_write_json, cache_dir, sha256_hex, USER_AGENT};
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const UPLOAD_BASE: &str = "https://kieai.redpandaai.co";
/// 60h — comfortably inside Kie's 3-day deletion window.
const URL_TTL_SECS: u64 = 60 * 60 * 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedUrl {
    url: String,
    expires_at: u64,
}

fn cache_path() -> std::path::PathBuf {
    cache_dir().join("uploads.json")
}

fn load_cache() -> BTreeMap<String, CachedUrl> {
    std::fs::read_to_string(cache_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn upload_base() -> String {
    std::env::var("KIE_UPLOAD_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| UPLOAD_BASE.to_string())
}

pub struct KieUploader {
    api_key: String,
    base: String,
    client: reqwest::Client,
}

impl KieUploader {
    pub fn new(api_key: String) -> Self {
        Self::with_base(api_key, upload_base())
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

    /// BYOK-or-gateway endpoint (dev-plan/53 Stage D). Gateway mode
    /// routes to `<gateway>/kie/api/file-stream-upload` — auth'd but
    /// free (Kie uploads cost nothing); the gateway swaps in the
    /// platform key and forwards to the upload host.
    pub fn resolve() -> Result<Self> {
        let ep = crate::media::provider::resolve_endpoint(&["KIE_API_KEY"], &upload_base(), "kie")?;
        Ok(Self::with_base(ep.api_key, ep.base_url))
    }

    /// Upload `path`, returning a Kie-fetchable URL. Content-hash
    /// cached; an unexpired hit costs no network at all.
    pub async fn upload(&self, path: &Path) -> Result<String> {
        let bytes = std::fs::read(path)
            .map_err(|e| Error::Tool(format!("read {}: {e}", path.display())))?;
        let key = sha256_hex(&bytes);

        let mut cache = load_cache();
        if let Some(hit) = cache.get(&key) {
            if hit.expires_at > now_epoch() {
                return Ok(hit.url.clone());
            }
        }

        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("asset.bin")
            .to_string();
        let part = reqwest::multipart::Part::bytes(bytes).file_name(name);
        let form = reqwest::multipart::Form::new()
            .text("uploadPath", "thclaws-film")
            .part("file", part);

        let resp: serde_json::Value = crate::multi_tenant::attach_member(
            self.client
                .post(format!("{}/api/file-stream-upload", self.base)),
        )
        .bearer_auth(&self.api_key)
        .multipart(form)
        .send()
        .await
        .map_err(|e| Error::Tool(format!("kie upload: {e}")))?
        .json()
        .await
        .map_err(|e| Error::Tool(format!("kie upload response: {e}")))?;

        let url = resp["data"]["fileUrl"]
            .as_str()
            .or_else(|| resp["data"]["downloadUrl"].as_str())
            .ok_or_else(|| {
                Error::Tool(format!(
                    "kie upload gave no url: {}",
                    truncate(&resp.to_string())
                ))
            })?
            .to_string();

        cache.insert(
            key,
            CachedUrl {
                url: url.clone(),
                expires_at: now_epoch() + URL_TTL_SECS,
            },
        );
        atomic_write_json(&cache_path(), &cache)?;
        Ok(url)
    }
}

fn truncate(s: &str) -> String {
    s.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_entries_miss() {
        let fresh = CachedUrl {
            url: "u".into(),
            expires_at: now_epoch() + 100,
        };
        let stale = CachedUrl {
            url: "u".into(),
            expires_at: now_epoch().saturating_sub(1),
        };
        assert!(fresh.expires_at > now_epoch());
        assert!(stale.expires_at <= now_epoch());
    }
}

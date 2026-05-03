//! HTTP client for `/api/print/*` endpoints. Auth is `Authorization: Bearer
//! <printer_token>`. Server returns 204 from `/jobs/next` when the queue is
//! empty — encoded here as `Ok(None)`.

use anyhow::{anyhow, Context, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::time::Duration;

const USER_AGENT: &str = concat!("lesscommerce-print-agent/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, Serialize)]
pub struct HeartbeatRequest<'a> {
    pub agent_version: &'a str,
    pub system_printer: &'a str,
    pub host: &'a str,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HeartbeatResponse {
    pub data: HeartbeatData,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HeartbeatData {
    // Server-issued UUID; not used by the agent today, but kept here so the
    // shape stays in sync with the BE contract and so future code (e.g. a
    // status command on the tray) can pick it up without re-deserializing.
    #[allow(dead_code)]
    pub printer_uuid: String,
    pub name: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub is_default: bool,
    #[serde(default = "default_poll")]
    pub poll_interval_seconds: u64,
}

fn default_poll() -> u64 {
    5
}

#[derive(Debug, Clone)]
pub struct NextJob {
    pub uuid: String,
    pub format: String,
    pub tracking: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Serialize)]
struct AckRequest<'a> {
    status: &'a str,
    error: Option<&'a str>,
    duration_ms: Option<u128>,
}

pub struct PrintApi {
    client: Client,
    api_url: String,
    token: String,
}

impl PrintApi {
    pub fn new(api_url: &str, token: &str) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(USER_AGENT)
            .build()
            .context("Failed to build HTTP client")?;
        Ok(Self {
            client,
            api_url: api_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
        })
    }

    pub async fn heartbeat(&self, system_printer: &str, host: &str) -> Result<HeartbeatData> {
        let body = HeartbeatRequest {
            agent_version: env!("CARGO_PKG_VERSION"),
            system_printer,
            host,
        };
        let response = self
            .client
            .post(format!("{}/api/print/heartbeat", self.api_url))
            .headers(self.auth_headers(true))
            .json(&body)
            .send()
            .await
            .context("Heartbeat transport error")?;

        if !response.status().is_success() {
            return Err(self.error_from(response).await);
        }

        let parsed: HeartbeatResponse = response.json().await.context("Heartbeat decode error")?;
        Ok(parsed.data)
    }

    /// Returns `Ok(None)` on 204 (empty queue) so the caller can sleep and
    /// retry. Any other status produces an error which the caller logs and
    /// backs off on.
    pub async fn fetch_next(&self) -> Result<Option<NextJob>> {
        let response = self
            .client
            .get(format!("{}/api/print/jobs/next", self.api_url))
            .headers(self.auth_headers(false))
            .send()
            .await
            .context("fetch_next transport error")?;

        if response.status() == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(self.error_from(response).await);
        }

        let uuid = read_header(response.headers(), "x-print-job-uuid").unwrap_or_default();
        let format =
            read_header(response.headers(), "x-print-job-format").unwrap_or_else(|| "pdf".into());
        let tracking = read_header(response.headers(), "x-print-job-tracking").unwrap_or_default();

        if uuid.is_empty() {
            return Err(anyhow!(
                "Server returned a job body but no X-Print-Job-Uuid header"
            ));
        }

        let bytes = response
            .bytes()
            .await
            .context("fetch_next: failed to read body")?
            .to_vec();
        Ok(Some(NextJob {
            uuid,
            format,
            tracking,
            bytes,
        }))
    }

    pub async fn ack_printed(&self, job_uuid: &str, duration_ms: u128) -> Result<()> {
        self.ack(job_uuid, "printed", None, Some(duration_ms)).await
    }

    pub async fn ack_failed(&self, job_uuid: &str, error: &str) -> Result<()> {
        self.ack(job_uuid, "failed", Some(error), None).await
    }

    async fn ack(
        &self,
        job_uuid: &str,
        status: &str,
        error: Option<&str>,
        duration_ms: Option<u128>,
    ) -> Result<()> {
        let body = AckRequest {
            status,
            error,
            duration_ms,
        };
        let response = self
            .client
            .post(format!("{}/api/print/jobs/{}/ack", self.api_url, job_uuid))
            .headers(self.auth_headers(true))
            .json(&body)
            .send()
            .await
            .context("ack transport error")?;
        if !response.status().is_success() {
            return Err(self.error_from(response).await);
        }
        Ok(())
    }

    fn auth_headers(&self, with_json_content: bool) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Ok(value) = HeaderValue::from_str(&format!("Bearer {}", self.token)) {
            headers.insert(AUTHORIZATION, value);
        }
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, application/pdf, application/octet-stream"),
        );
        if with_json_content {
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        }
        headers
    }

    async fn error_from(&self, response: reqwest::Response) -> anyhow::Error {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow!("HTTP {}: {}", status, truncate(&body, 400))
    }
}

fn read_header(headers: &HeaderMap, name: &str) -> Option<String> {
    let header_name = HeaderName::from_bytes(name.as_bytes()).ok()?;
    headers
        .get(header_name)?
        .to_str()
        .ok()
        .map(|s| s.to_string())
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

//! HTTP client for the runner API (`crates/proto`).

use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use reqwest::StatusCode;
use rust_afariano_proto::{JobResult, JobSpec, LogAck, REQUEST_LONG_POLL_SECS, RunnerHello, RunnerInfo};

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    runner_token: String,
}

/// Outcome of a log `PUT`.
#[derive(Debug, Clone, Copy)]
pub enum LogPut {
    Ok(LogAck),
    /// Offset mismatch: the server holds `size` bytes.
    Conflict(LogAck),
    /// The job token is no longer valid (job finished or canceled server-side).
    Gone,
}

impl Client {
    pub fn new(base: &str, runner_token: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("rust-afariano-runner/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .build()
            .context("building HTTP client")?;
        Ok(Self { http, base: base.trim_end_matches('/').to_string(), runner_token: runner_token.to_string() })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    pub async fn verify(&self, hello: &RunnerHello) -> Result<RunnerInfo> {
        let resp = self
            .http
            .post(self.url("/api/v1/runner/verify"))
            .bearer_auth(&self.runner_token)
            .timeout(Duration::from_secs(30))
            .json(hello)
            .send()
            .await
            .with_context(|| format!("contacting {}", self.base))?;
        let status = resp.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            bail!("the server rejected the runner token (HTTP {status})");
        }
        if !status.is_success() {
            bail!("verify failed: HTTP {status}: {}", body_snippet(resp).await);
        }
        resp.json().await.context("decoding verify reply")
    }

    /// Long poll for a job. `Ok(None)` = nothing to do.
    pub async fn request_job(&self, hello: &RunnerHello) -> Result<Option<JobSpec>, RequestError> {
        let resp = self
            .http
            .post(self.url("/api/v1/runner/jobs/request"))
            .bearer_auth(&self.runner_token)
            .timeout(Duration::from_secs(REQUEST_LONG_POLL_SECS + 20))
            .json(hello)
            .send()
            .await
            .map_err(|e| RequestError::Transient(anyhow!(e)))?;
        match resp.status() {
            StatusCode::NO_CONTENT => Ok(None),
            StatusCode::OK => {
                resp.json().await.map(Some).map_err(|e| RequestError::Transient(anyhow!("decoding job: {e}")))
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(RequestError::Unauthorized),
            s => Err(RequestError::Transient(anyhow!("HTTP {s}: {}", body_snippet(resp).await))),
        }
    }

    pub async fn put_log(&self, job_id: i64, job_token: &str, offset: u64, data: Vec<u8>) -> Result<LogPut> {
        let resp = self
            .http
            .put(self.url(&format!("/api/v1/runner/jobs/{job_id}/log?offset={offset}")))
            .bearer_auth(job_token)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .timeout(Duration::from_secs(60))
            .body(data)
            .send()
            .await?;
        match resp.status() {
            s if s.is_success() => Ok(LogPut::Ok(resp.json().await?)),
            StatusCode::CONFLICT => Ok(LogPut::Conflict(resp.json().await?)),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::GONE => {
                Ok(LogPut::Gone)
            }
            s => bail!("HTTP {s}: {}", body_snippet(resp).await),
        }
    }

    /// `Ok(true)` = accepted, `Ok(false)` = the server no longer knows the job token.
    pub async fn finish(&self, job_id: i64, job_token: &str, result: &JobResult) -> Result<bool> {
        let resp = self
            .http
            .post(self.url(&format!("/api/v1/runner/jobs/{job_id}/finish")))
            .bearer_auth(job_token)
            .timeout(Duration::from_secs(60))
            .json(result)
            .send()
            .await?;
        match resp.status() {
            s if s.is_success() => Ok(true),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::GONE => Ok(false),
            s => bail!("HTTP {s}: {}", body_snippet(resp).await),
        }
    }
}

#[derive(Debug)]
pub enum RequestError {
    Unauthorized,
    Transient(anyhow::Error),
}

async fn body_snippet(resp: reqwest::Response) -> String {
    let text = resp.text().await.unwrap_or_default();
    let mut s: String = text.chars().take(300).collect();
    if s.len() < text.len() {
        s.push('…');
    }
    s
}

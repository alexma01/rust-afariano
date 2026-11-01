//! Wire types shared by `rust-afariano` and `rust-afariano-runner`.
//!
//! Every runner call is JSON over HTTPS, started by the runner (runners never
//! accept connections, so they work behind NAT):
//!
//! | Call | Auth (`Authorization: Bearer …`) | Body → reply |
//! |---|---|---|
//! | `POST /api/v1/runner/verify` | runner token | [`RunnerHello`] → [`RunnerInfo`] |
//! | `POST /api/v1/runner/jobs/request` | runner token | [`RunnerHello`] → `200` [`JobSpec`] or `204` (long poll, ≤ [`REQUEST_LONG_POLL_SECS`]) |
//! | `PUT /api/v1/runner/jobs/{id}/log?offset=N` | job token | raw log bytes → [`LogAck`] (`409` + [`LogAck`] when `N` ≠ server size) |
//! | `POST /api/v1/runner/jobs/{id}/finish` | job token | [`JobResult`] → `204` |
//!
//! The runner clones with HTTPS basic auth: user [`CI_GIT_USER`], password = job token.

use serde::{Deserialize, Serialize};

/// Prefix of runner authentication tokens.
pub const RUNNER_TOKEN_PREFIX: &str = "rfr_";
/// Prefix of per-job tokens (valid only while the job runs).
pub const JOB_TOKEN_PREFIX: &str = "rfj_";
/// Prefix of personal access tokens.
pub const PAT_PREFIX: &str = "rfp_";
/// Username a runner uses for git over HTTPS (password = job token).
pub const CI_GIT_USER: &str = "rust-afariano-ci";
/// The server answers `jobs/request` within this many seconds.
pub const REQUEST_LONG_POLL_SECS: u64 = 25;
/// Largest log chunk the server accepts in one `PUT`.
pub const MAX_LOG_CHUNK: usize = 1024 * 1024;
/// A job log is cut (and the job keeps running) beyond this size.
pub const MAX_LOG_SIZE: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerHello {
    /// `rust-afariano-runner` version.
    pub version: String,
    /// e.g. `macos-aarch64`, `linux-x86_64`.
    pub platform: String,
    /// `docker` or `shell`.
    pub executor: String,
    pub hostname: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerInfo {
    pub id: i64,
    pub name: String,
    pub tags: Vec<String>,
    pub run_untagged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSpec {
    pub name: String,
    /// `None` keeps the image's entrypoint; `Some(vec![])` (from `entrypoint: [""]`) clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceSpec {
    pub name: String,
    /// Extra network aliases (`alias:` may list several, comma or space separated).
    /// The runner always adds the ones derived from the image name as GitLab does.
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// Service-only variables (`services: [{variables: …}]`), on top of the job's.
    #[serde(default)]
    pub variables: Vec<Variable>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Variable {
    pub key: String,
    pub value: String,
    /// File variable: the runner writes `value` to a file and sets `key` to its path.
    #[serde(default)]
    pub file: bool,
    /// Masked: the runner replaces `value` with `[MASKED]` in the log.
    #[serde(default)]
    pub masked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitSpec {
    /// HTTPS clone URL without credentials, e.g. `https://git.example.com/team/website.git`.
    pub url: String,
    pub sha: String,
    /// Branch or tag name.
    pub ref_name: String,
    pub ref_is_tag: bool,
    /// `GIT_DEPTH`; 0 = full history.
    pub depth: u32,
    /// `GIT_STRATEGY`: `fetch` (default), `clone` or `none`.
    pub strategy: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    pub id: i64,
    /// Job token: bearer for the log/finish calls and git password.
    pub token: String,
    pub name: String,
    pub stage: String,
    pub pipeline_id: i64,
    /// `namespace/name`; the build directory is `<builds_dir>/<project_path>`.
    pub project_path: String,
    pub git: GitSpec,
    /// `None`: runner default image (docker) / ignored (shell).
    pub image: Option<ImageSpec>,
    pub services: Vec<ServiceSpec>,
    pub before_script: Vec<String>,
    pub script: Vec<String>,
    pub after_script: Vec<String>,
    /// Everything the job sees, already merged by precedence (predefined,
    /// YAML global, YAML job, project, pipeline). The runner adds
    /// `CI_PROJECT_DIR`, `CI_BUILDS_DIR` and `CI_RUNNER_*` itself.
    pub variables: Vec<Variable>,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct LogAck {
    /// Bytes the server holds for this log; the next `PUT` must use it as `offset`.
    pub size: u64,
    /// The job was canceled from the UI: stop it and finish with `canceled`.
    pub cancel: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobOutcome {
    Success,
    Failed,
    Canceled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureReason {
    ScriptFailure,
    JobTimeout,
    RunnerSystemFailure,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobResult {
    pub outcome: JobOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<FailureReason>,
}

impl FailureReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ScriptFailure => "script_failure",
            Self::JobTimeout => "job_timeout",
            Self::RunnerSystemFailure => "runner_system_failure",
        }
    }
}

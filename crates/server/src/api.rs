//! JSON API for scripts, authenticated with a personal access token
//! (`Authorization: Bearer rfp_…`). Errors are `{"message": …}`.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use crate::app::Shared;
use crate::auth::TokenUser2;
use crate::error::{AppError, AppResult, bad};
use crate::models::{JOB_COLS, Job, PIPELINE_COLS, Pipeline};
use crate::ops;

#[derive(Deserialize)]
pub struct NewRepo {
    namespace: String,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    default_branch: Option<String>,
}

/// `POST /api/v1/repos` `{"namespace", "name", "description"?, "default_branch"?}`
pub async fn create_repo(
    State(state): State<Shared>,
    TokenUser2(user): TokenUser2,
    Json(b): Json<NewRepo>,
) -> AppResult<Response> {
    let ns = b.namespace.trim();
    let name = b.name.trim();
    let branch = b.default_branch.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("main");
    ops::create_repo(&state, &user, ns, name, &b.description, branch).await?.map_err(bad)?;
    let path = format!("{ns}/{name}");
    let cfg = &state.cfg;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "path": path,
            "web_url": format!("{}/{path}", cfg.public_url),
            "http_url": cfg.http_clone_url(&path),
            "ssh_url": cfg.ssh_clone_url(&path),
        })),
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct VariableBody {
    value: String,
    #[serde(default)]
    file: bool,
    #[serde(default)]
    protected: bool,
    #[serde(default)]
    masked: bool,
    #[serde(default)]
    environment_scope: String,
}

/// `PUT /api/v1/repos/{ns}/{name}/variables/{key}` — create or replace (per scope).
pub async fn put_variable(
    State(state): State<Shared>,
    TokenUser2(_user): TokenUser2,
    Path((ns, name, key)): Path<(String, String, String)>,
    Json(b): Json<VariableBody>,
) -> AppResult<StatusCode> {
    let repo = state.repo_by_path(&ns, &name).await?.ok_or(AppError::NotFound)?;
    let input = ops::VariableInput {
        key,
        value: b.value,
        file: b.file,
        protected: b.protected,
        masked: b.masked,
        environment_scope: b.environment_scope,
    };
    ops::upsert_variable(&state, repo.id, input).await?.map_err(bad)?;
    Ok(StatusCode::NO_CONTENT)
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
pub struct NewRunner {
    name: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default = "yes")]
    run_untagged: bool,
}

/// `POST /api/v1/runners` `{"name", "tags"?, "run_untagged"?}` → `{"id", "token"}` (token shown once).
pub async fn create_runner(
    State(state): State<Shared>,
    TokenUser2(user): TokenUser2,
    Json(b): Json<NewRunner>,
) -> AppResult<Response> {
    if !user.is_admin {
        return Err(AppError::Forbidden);
    }
    let tags = ops::parse_tags(b.tags.iter().map(String::as_str));
    let (id, token) = ops::create_runner(&state, &user, &b.name, tags, b.run_untagged).await?.map_err(bad)?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id, "token": token }))).into_response())
}

/// `GET /api/v1/repos/{ns}/{name}/pipelines/{iid}` — status and jobs.
pub async fn get_pipeline(
    State(state): State<Shared>,
    TokenUser2(_user): TokenUser2,
    Path((ns, name, iid)): Path<(String, String, i32)>,
) -> AppResult<Json<serde_json::Value>> {
    let repo = state.repo_by_path(&ns, &name).await?.ok_or(AppError::NotFound)?;
    let p: Pipeline = sqlx::query_as(sql!("SELECT {PIPELINE_COLS} FROM pipelines WHERE repo_id = $1 AND iid = $2"))
        .bind(repo.id)
        .bind(iid)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let jobs: Vec<Job> = sqlx::query_as(sql!(
        "SELECT {JOB_COLS} FROM jobs WHERE pipeline_id = $1 AND NOT retried ORDER BY stage_idx, id"
    ))
    .bind(p.id)
    .fetch_all(&state.db)
    .await?;
    let base = format!("{}/{}", state.cfg.public_url, repo.path());
    Ok(Json(json!({
        "iid": p.iid,
        "status": p.status,
        "ref": p.ref_name,
        "sha": p.sha,
        "source": p.source,
        "error": p.error,
        "web_url": format!("{base}/-/pipelines/{}", p.iid),
        "jobs": jobs.iter().map(|j| json!({
            "id": j.id,
            "name": j.name,
            "stage": j.stage,
            "status": j.status,
            "failure_reason": j.failure_reason,
            "web_url": format!("{base}/-/jobs/{}", j.id),
        })).collect::<Vec<_>>(),
    })))
}

//! Operations shared by the web UI and the JSON API, so both validate the same way.
//! `Ok(Err(msg))` is a user mistake to show; `Err` is an internal failure.

use rust_afariano_proto as proto;

use crate::app::AppState;
use crate::error::AppResult;
use crate::models::User;
use crate::util::{hash_token, new_token, reserved_namespace, valid_slug};
use crate::web::valid_var_key;

pub type Checked<T> = AppResult<Result<T, &'static str>>;

pub fn valid_branch(b: &str) -> bool {
    !b.is_empty()
        && b.len() <= 200
        && !b.starts_with(['-', '/', '.'])
        && !b.ends_with(['/', '.'])
        && !b.ends_with(".lock")
        && !b.contains("..")
        && !b.contains("//")
        && !b.contains("@{")
        && b.chars().all(|c| !c.is_control() && !" ~^:?*[\\".contains(c))
}

/// Creates the row and the bare repository; returns the repo id.
pub async fn create_repo(
    state: &AppState,
    user: &User,
    ns: &str,
    name: &str,
    description: &str,
    branch: &str,
) -> Checked<i64> {
    if !valid_slug(ns) || reserved_namespace(ns) {
        return Ok(Err("Namespace: letters, digits, `.`, `_`, `-` (not a reserved word)."));
    }
    if !valid_slug(name) {
        return Ok(Err("Name: letters, digits, `.`, `_`, `-`; must not end in `.git`."));
    }
    if !valid_branch(branch) {
        return Ok(Err("Default branch name is not valid."));
    }
    if state.repo_by_path(ns, name).await?.is_some() {
        return Ok(Err("A repository with that path already exists."));
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO repos (namespace, name, description, default_branch, protected_branches, created_by) \
         VALUES ($1, $2, $3, $4, $4, $5) RETURNING id",
    )
    .bind(ns)
    .bind(name)
    .bind(description.trim())
    .bind(branch)
    .bind(user.id)
    .fetch_one(&state.db)
    .await?;
    if let Err(e) = state.git(id).init(branch).await {
        sqlx::query("DELETE FROM repos WHERE id = $1").bind(id).execute(&state.db).await?;
        return Err(e.into());
    }
    tracing::info!(repo = %format!("{ns}/{name}"), user = %user.email, "repository created");
    Ok(Ok(id))
}

pub struct VariableInput {
    pub key: String,
    pub value: String,
    pub file: bool,
    pub protected: bool,
    pub masked: bool,
    pub environment_scope: String,
}

/// Insert or replace (same key + scope) a CI/CD variable.
pub async fn upsert_variable(state: &AppState, repo_id: i64, v: VariableInput) -> Checked<String> {
    let key = v.key.trim().to_string();
    let scope =
        if v.environment_scope.trim().is_empty() { "*".to_string() } else { v.environment_scope.trim().to_string() };
    // Browsers send CRLF from textareas; secrets (keys, env files) want LF.
    let mut value = v.value.replace("\r\n", "\n");
    if v.file && !value.is_empty() && !value.ends_with('\n') {
        value.push('\n'); // OpenSSH refuses a private key file without the final newline
    }
    if !valid_var_key(&key) {
        return Ok(Err("Key: letters, digits and _, not starting with a digit."));
    }
    if v.masked && (value.trim().len() < 8 || value.trim().contains('\n')) {
        return Ok(Err("A masked value must be a single line of at least 8 characters."));
    }
    sqlx::query(
        "INSERT INTO variables (repo_id, key, value, file, protected, masked, environment_scope) VALUES ($1,$2,$3,$4,$5,$6,$7) \
         ON CONFLICT (repo_id, key, environment_scope) DO UPDATE SET value = EXCLUDED.value, file = EXCLUDED.file, \
         protected = EXCLUDED.protected, masked = EXCLUDED.masked",
    )
    .bind(repo_id)
    .bind(&key)
    .bind(state.cipher.encrypt(&value))
    .bind(v.file)
    .bind(v.protected)
    .bind(v.masked)
    .bind(&scope)
    .execute(&state.db)
    .await?;
    Ok(Ok(key))
}

pub fn parse_tags<'a>(tags: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = tags
        .into_iter()
        .flat_map(|t| t.split([',', ' ']))
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Creates a runner; returns `(id, token)`. The token is shown once and stored hashed.
pub async fn create_runner(
    state: &AppState,
    user: &User,
    name: &str,
    tags: Vec<String>,
    run_untagged: bool,
) -> Checked<(i64, String)> {
    let name = name.trim();
    if name.is_empty() || name.len() > 100 {
        return Ok(Err("Give the runner a name (up to 100 characters)."));
    }
    let token = new_token(proto::RUNNER_TOKEN_PREFIX);
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO runners (name, token_hash, tags, run_untagged, created_by) VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(name)
    .bind(hash_token(&token))
    .bind(&tags)
    .bind(run_untagged)
    .bind(user.id)
    .fetch_one(&state.db)
    .await?;
    tracing::info!(runner = name, user = %user.email, "runner created");
    Ok(Ok((id, token)))
}

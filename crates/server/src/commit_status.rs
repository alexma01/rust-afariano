//! Pipeline results sent back to a mirror's upstream as commit statuses
//! (GitHub's commit statuses, Bitbucket's build statuses), so a commit or pull
//! request there shows whether its pipeline passed.
//!
//! Pipelines only queue their id here; one worker sends the updates in order,
//! reading each pipeline's status at send time, so a quick run of changes
//! collapses into the latest one and an older status never overwrites a newer.

use base64::prelude::*;
use serde_json::{Value, json};

use crate::app::{AppState, Shared};
use crate::mirror::{Forge, Upstream};

/// Queues a status update for the pipeline's upstream (a no-op for ordinary repositories).
pub fn notify(state: &AppState, pipeline_id: i64) {
    let _ = state.status_tx.send(pipeline_id);
}

pub async fn worker(state: Shared) {
    let Some(mut rx) = state.status_rx.lock().expect("status queue").take() else { return };
    while let Some(pipeline_id) = rx.recv().await {
        if let Err(e) = report(&state, pipeline_id).await {
            tracing::warn!(pipeline_id, "commit status: {e:#}");
        }
    }
}

/// What a forge is told for a pipeline status.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    github: &'static str,
    bitbucket: &'static str,
    words: &'static str,
}

fn outcome(status: &str) -> Option<Outcome> {
    let (github, bitbucket, words) = match status {
        "created" | "pending" => ("pending", "INPROGRESS", "is waiting for a runner"),
        "running" => ("pending", "INPROGRESS", "is running"),
        "manual" => ("pending", "INPROGRESS", "is waiting for a manual job"),
        "success" => ("success", "SUCCESSFUL", "passed"),
        "skipped" => ("success", "SUCCESSFUL", "was skipped"),
        "failed" => ("failure", "FAILED", "failed"),
        "canceled" => ("error", "STOPPED", "was canceled"),
        _ => return None,
    };
    Some(Outcome { github, bitbucket, words })
}

pub struct PipelineFacts<'a> {
    pub iid: i32,
    pub sha: &'a str,
    pub ref_name: &'a str,
    pub status: &'a str,
    pub web_url: String,
    /// The server's host, e.g. `pipe.molecle.com`: names the check on the forge.
    pub server: &'a str,
}

#[derive(Debug, PartialEq)]
pub struct Request {
    pub url: String,
    pub authorization: String,
    pub body: Value,
}

/// A token alone goes as `Bearer` (GitHub tokens, Bitbucket repository access tokens);
/// `user:token` as Basic (Bitbucket API tokens with the account email).
fn authorization(token: &str) -> String {
    match token.split_once(':') {
        Some(_) => format!("Basic {}", BASE64_STANDARD.encode(token)),
        None => format!("Bearer {token}"),
    }
}

fn build(forge: &Forge, token: &str, p: &PipelineFacts<'_>) -> Option<Request> {
    let o = outcome(p.status)?;
    let description = format!("Pipeline #{} {}", p.iid, o.words);
    Some(match forge {
        Forge::GitHub { api, repo } => Request {
            url: format!("{api}/repos/{repo}/statuses/{}", p.sha),
            authorization: authorization(token),
            body: json!({
                "state": o.github,
                "target_url": p.web_url,
                "description": description,
                "context": p.server,
            }),
        },
        Forge::Bitbucket { api, repo } => Request {
            url: format!("{api}/repositories/{repo}/commit/{}/statuses/build", p.sha),
            authorization: authorization(token),
            body: json!({
                // Bitbucket identifies a status by its key: one per server.
                "key": p.server.chars().take(40).collect::<String>(),
                "state": o.bitbucket,
                "name": format!("{} #{}", p.server, p.iid),
                "url": p.web_url,
                "description": description,
                "refname": p.ref_name,
            }),
        },
    })
}

async fn send(http: &reqwest::Client, req: &Request) -> anyhow::Result<()> {
    let resp = http
        .post(&req.url)
        .header(reqwest::header::AUTHORIZATION, &req.authorization)
        .header(reqwest::header::ACCEPT, "application/json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .json(&req.body)
        .send()
        .await?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let text = resp.text().await.unwrap_or_default();
    let detail: String = text.chars().take(300).collect();
    let hint = match status.as_u16() {
        401 => " (the token is not valid or expired)",
        403 | 404 => " (the token may not write commit statuses on this repository)",
        _ => "",
    };
    anyhow::bail!("{} answered {status}{hint}: {detail}", req.url.split('/').take(3).collect::<Vec<_>>().join("/"))
}

#[derive(sqlx::FromRow)]
struct Row {
    iid: i32,
    sha: String,
    ref_name: String,
    status: String,
    reported_status: Option<String>,
    namespace: String,
    name: String,
    mirror_url: String,
    repo_id: i64,
    status_token: String,
}

async fn report(state: &AppState, pipeline_id: i64) -> anyhow::Result<()> {
    let row: Option<Row> = sqlx::query_as(
        "SELECT p.iid, p.sha, p.ref_name, p.status, p.reported_status, r.namespace, r.name, r.mirror_url, r.id AS repo_id, \
         m.status_token FROM pipelines p JOIN repos r ON r.id = p.repo_id JOIN mirrors m ON m.repo_id = r.id \
         WHERE p.id = $1 AND r.mirror_url IS NOT NULL AND m.status_token IS NOT NULL",
    )
    .bind(pipeline_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(row) = row else { return Ok(()) };
    if row.reported_status.as_deref() == Some(row.status.as_str()) {
        return Ok(());
    }
    let result = async {
        let up = Upstream::parse(&row.mirror_url).map_err(|e| anyhow::anyhow!("{e}"))?;
        let Some(forge) = up.forge() else {
            anyhow::bail!("commit statuses work with github.com, *.ghe.com and bitbucket.org, not {}", up.host);
        };
        let token = state.cipher.decrypt(&row.status_token)?;
        let facts = PipelineFacts {
            iid: row.iid,
            sha: &row.sha,
            ref_name: &row.ref_name,
            status: &row.status,
            web_url: format!("{}/{}/{}/-/pipelines/{}", state.cfg.public_url, row.namespace, row.name, row.iid),
            server: state.cfg.public_host(),
        };
        let Some(req) = build(&forge, token.trim(), &facts) else { return Ok(()) };
        send(&state.http, &req).await
    }
    .await;
    let error = result.as_ref().err().map(|e| format!("{e:#}"));
    if error.is_none() {
        sqlx::query("UPDATE pipelines SET reported_status = $2 WHERE id = $1")
            .bind(pipeline_id)
            .bind(&row.status)
            .execute(&state.db)
            .await?;
    }
    sqlx::query("UPDATE mirrors SET status_at = now(), status_error = $2 WHERE repo_id = $1")
        .bind(row.repo_id)
        .bind(&error)
        .execute(&state.db)
        .await?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(status: &str) -> PipelineFacts<'_> {
        PipelineFacts {
            iid: 12,
            sha: "0123456789abcdef0123456789abcdef01234567",
            ref_name: "main",
            status,
            web_url: "https://pipe.molecle.com/devops/davegantt/-/pipelines/12".into(),
            server: "pipe.molecle.com",
        }
    }

    #[test]
    fn github_request() {
        let forge =
            Forge::GitHub { api: "https://api.github.com".into(), repo: "Matteo7S/lead-capture-platform".into() };
        let req = build(&forge, "github_pat_x", &facts("running")).unwrap();
        assert_eq!(
            req.url,
            "https://api.github.com/repos/Matteo7S/lead-capture-platform/statuses/0123456789abcdef0123456789abcdef01234567"
        );
        assert_eq!(req.authorization, "Bearer github_pat_x");
        assert_eq!(req.body["state"], "pending");
        assert_eq!(req.body["context"], "pipe.molecle.com");
        assert_eq!(req.body["description"], "Pipeline #12 is running");
        assert_eq!(build(&forge, "t", &facts("failed")).unwrap().body["state"], "failure");
        assert_eq!(build(&forge, "t", &facts("canceled")).unwrap().body["state"], "error");
    }

    #[test]
    fn bitbucket_request() {
        let forge = Forge::Bitbucket { api: "https://api.bitbucket.org/2.0".into(), repo: "molecle/davegantt".into() };
        let req = build(&forge, "ATCTT3x", &facts("success")).unwrap();
        assert_eq!(
            req.url,
            "https://api.bitbucket.org/2.0/repositories/molecle/davegantt/commit/0123456789abcdef0123456789abcdef01234567/statuses/build"
        );
        assert_eq!(req.body["state"], "SUCCESSFUL");
        assert_eq!(req.body["key"], "pipe.molecle.com");
        assert_eq!(req.body["refname"], "main");
        // An API token with the account email goes as Basic auth.
        let basic = build(&forge, "ana@example.com:ATATT", &facts("success")).unwrap();
        assert_eq!(basic.authorization, format!("Basic {}", BASE64_STANDARD.encode("ana@example.com:ATATT")));
        assert!(build(&forge, "t", &facts("weird")).is_none());
    }

    /// The request reaches the forge as built (a local stand-in for its API).
    #[tokio::test]
    async fn sends_to_the_api() {
        use std::sync::{Arc, Mutex};

        use axum::extract::State;
        use axum::http::HeaderMap;
        type Seen = Arc<Mutex<Vec<(String, String, Value)>>>;
        let seen: Seen = Arc::default();
        let app = axum::Router::new()
            .route(
                "/repos/{owner}/{repo}/statuses/{sha}",
                axum::routing::post(
                    |State(seen): State<Seen>,
                     axum::extract::Path((owner, repo, sha)): axum::extract::Path<(String, String, String)>,
                     headers: HeaderMap,
                     axum::Json(body): axum::Json<Value>| async move {
                        let auth = headers["authorization"].to_str().unwrap().to_string();
                        seen.lock().unwrap().push((format!("{owner}/{repo}@{sha}"), auth, body));
                        (axum::http::StatusCode::CREATED, "{}")
                    },
                ),
            )
            .route(
                "/repos/denied/x/statuses/{sha}",
                axum::routing::post(|| async { axum::http::StatusCode::NOT_FOUND }),
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let http = reqwest::Client::new();
        let forge = Forge::GitHub { api: format!("http://{addr}"), repo: "acme/web".into() };
        send(&http, &build(&forge, "tok", &facts("success")).unwrap()).await.unwrap();
        let got = seen.lock().unwrap().clone();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "acme/web@0123456789abcdef0123456789abcdef01234567");
        assert_eq!(got[0].1, "Bearer tok");
        assert_eq!(got[0].2["state"], "success");

        let denied = Forge::GitHub { api: format!("http://{addr}"), repo: "denied/x".into() };
        let err = send(&http, &build(&denied, "tok", &facts("success")).unwrap()).await.unwrap_err();
        assert!(format!("{err}").contains("may not write commit statuses"), "{err}");
    }
}
